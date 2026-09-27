//! Converts eslint-plugin-import / eslint-plugin-import-x dependency rules:
//! `no-cycle`, `no-restricted-paths` and `no-extraneous-dependencies`, from
//! flat (`eslint.config.*`) or legacy (`.eslintrc*` in JSON, YAML or JS,
//! package.json `eslintConfig`) configs, honouring `files` / `ignores` /
//! `overrides` scoping, legacy `extends` chains and the `import/resolver`
//! settings.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use super::glob::{self, Part, Plain, has_magic, to_regex};
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
const boundaries = (s) => s && Object.keys(s).some((k) => k.startsWith('boundaries/')) ? {
  elements: s['boundaries/elements'] || null, ignore: s['boundaries/ignore'] || null, include: s['boundaries/include'] || null,
  nodes: s['boundaries/dependency-nodes'] || null, files: s['boundaries/files'] || null,
} : null;
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

    // boundaries settings (per key, the last definition wins, as settings merge).
    let items = v["items"].as_array().cloned().unwrap_or_default();
    let setting = |key: &str| items.iter().filter_map(|i| i["boundaries"].get(key).filter(|v| !v.is_null()).cloned()).next_back();
    let mut ctx = Ctx {
        prefix: prefix.clone(),
        elements: setting("elements").and_then(|e| e.as_array().cloned()).unwrap_or_default(),
        parsed: vec![],
        ignore: setting("ignore").map(|v| strs(&v)).unwrap_or_default(),
        include: setting("include").map(|v| strs(&v)).unwrap_or_default(),
        nodes: setting("nodes"),
    };
    ctx.parsed = boundary_elements(&ctx);
    if setting("files").is_some() {
        warnings.push("boundaries/files (file descriptors) aren't converted".into());
    }
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
            const BOUNDARIES: &[&str] = &["element-types", "dependencies", "entry-point", "external", "no-unknown", "no-unknown-dependencies"];
            let boundary_rule = name.starts_with("boundaries/") && BOUNDARIES.contains(&short.as_str());
            if name.starts_with("boundaries/") && !boundary_rule {
                if sev != Severity::Off {
                    warnings.push(format!("{name} isn't converted"));
                }
                continue;
            }
            if boundary_rule && sev != Severity::Off && groups.is_empty() {
                groups = boundary_groups(&ctx, &ctx.parsed);
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
    if !groups.is_empty() {
        // Like the plugin, a file belongs to its innermost element.
        config.options.group_match = crate::config::GroupMatch::Deepest;
    }
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
    /// `boundaries/elements` settings, raw and parsed.
    elements: Vec<Value>,
    parsed: Vec<Element>,
    /// `boundaries/ignore` / `boundaries/include` globs.
    ignore: Vec<String>,
    include: Vec<String>,
    /// `boundaries/dependency-nodes`.
    nodes: Option<Value>,
}

const NPM_TYPES: &[&str] = &["npm", "npm-dev", "npm-peer", "npm-optional", "npm-undeclared"];

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|s| s.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

/// Nx `allow` / `checkDynamicDependenciesExceptions` entries, as a regex on
/// the specifier (`matchImportWithWildcard`).
fn nx_allow(a: &str) -> String {
    let esc = |s: &str| fancy_regex::escape(s).into_owned();
    if let Some(p) = a.strip_suffix("/**") {
        format!("^{}/", esc(p))
    } else if let Some(p) = a.strip_suffix("/*") {
        format!("^{}/[^/]*$", esc(p))
    } else if let Some((pre, suf)) = a.split_once("/**/") {
        format!("^(?={})(?=.*{}$)", esc(pre), esc(suf))
    } else {
        // Anything else is used as an (unanchored) regular expression.
        a.to_string()
    }
}

/// Nx `bannedExternalImports` / `allowedExternalImports` entries, as a regex
/// on the specifier (`mapGlobToRegExp`: `*` and `.*` are wildcards, the rest
/// is used as a regex).
fn nx_external(g: &str) -> String {
    let wild = regex::Regex::new(r"\.\*|\*+").expect("valid");
    format!("^(?:{})$", wild.split(g).collect::<Vec<_>>().join(".*"))
}

/// `@nx/enforce-module-boundaries`: projects come from Nx discovery
/// (`options.nx_projects`). Each check becomes group rules (between projects)
/// or module rules (about single imports). Like Nx, only `import` / `export
/// … from` / `import()` are checked, not `require()`, and `allow`ed
/// specifiers are exempt from everything.
fn convert_nx(full: &str, sev: Severity, o: &Value, warnings: &mut Vec<String>) -> Vec<Rule> {
    let list = |key: &str| strings(o.get(key).unwrap_or(&Value::Null));
    let allow = list("allow");
    let allow_re = (!allow.is_empty()).then(|| Pat::any(&allow.iter().map(|a| nx_allow(a)).collect::<Vec<_>>()));
    let in_project = || FromSpec { tags: Some(vec!["projectType:*".into()]), ..Default::default() };
    let types = |l: &[&str]| Some(l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let finish = |mut r: Rule| {
        r.to.specifier_not = match (r.to.specifier_not.take(), &allow_re) {
            (Some(a), Some(b)) => Some(or(Some(a), b.clone())),
            (a, b) => a.or_else(|| b.clone()),
        };
        let not = r.to.dependency_types_not.get_or_insert_with(Vec::new);
        if !not.iter().any(|t| t == "require") {
            not.push("require".into());
        }
        r
    };
    let mut out = vec![
        group_rule(full, sev, "Circular dependency between projects.", FromSpec::default(), ToSpec { circular: Some(true), ..Default::default() }),
        group_rule(
            full,
            sev,
            "Imports of apps and e2e projects are forbidden.",
            FromSpec::default(),
            ToSpec { tags: Some(vec!["projectType:application".into()]), ..Default::default() },
        ),
        rule(
            full,
            sev,
            "Projects cannot be imported by a relative or absolute path, and must begin with an npm scope.",
            FromSpec::default(),
            ToSpec { dependency_types: types(&["relative"]), cross_group: Some(true), ..Default::default() },
        ),
        rule(
            full,
            sev,
            "External resources cannot be imported using a relative or absolute path.",
            in_project(),
            ToSpec { dependency_types: types(&["relative"]), tags_not: Some(vec!["*".into()]), ..Default::default() },
        ),
    ];
    if o.get("allowCircularSelfDependency") != Some(&json!(true)) {
        out.push(rule(
            full,
            sev,
            "Projects should use relative imports to import from other files within the same project.",
            in_project(),
            ToSpec { dependency_types_not: types(&["relative"]), cross_group: Some(false), ..Default::default() },
        ));
    }
    // Static imports of a project the source also loads with import().
    let exceptions = list("checkDynamicDependenciesExceptions");
    out.push(group_rule(
        full,
        sev,
        "Static imports of lazy-loaded libraries are forbidden.",
        FromSpec::default(),
        ToSpec {
            lazy_loaded: Some(true),
            dependency_types_not: types(&["dynamic", "reexport", "type-only", "resource"]),
            specifier_not: (!exceptions.is_empty()).then(|| Pat::any(&exceptions.iter().map(|a| nx_allow(a)).collect::<Vec<_>>())),
            ..Default::default()
        },
    ));
    if o.get("banTransitiveDependencies") == Some(&json!(true)) {
        let msg = "Only packages defined in the \"package.json\" can be imported. Transitive or unresolvable dependencies are not allowed.";
        out.push(rule(full, sev, msg, in_project(), ToSpec { dependency_types: types(&["npm-undeclared"]), ..Default::default() }));
        out.push(rule(
            full,
            sev,
            msg,
            in_project(),
            ToSpec { could_not_resolve: Some(true), dependency_types_not: types(&["relative"]), ..Default::default() },
        ));
        // A bare import of a local file outside every project.
        out.push(rule(
            full,
            sev,
            msg,
            in_project(),
            ToSpec { dependency_types: types(&["aliased"]), tags_not: Some(vec!["*".into()]), ..Default::default() },
        ));
    }
    if o.get("enforceBuildableLibDependency") == Some(&json!(true)) {
        let build = match o.get("buildTargets") {
            Some(t) => strings(t),
            None => vec!["build".into()],
        };
        let targets: Vec<String> = build.iter().map(|t| format!("target:{t}")).collect();
        out.push(group_rule(
            full,
            sev,
            "Buildable libraries cannot import or export from non-buildable libraries.",
            FromSpec { tags_all: Some(vec!["projectType:library".into()]), tags: Some(targets.clone()), ..Default::default() },
            ToSpec { tags: Some(vec!["projectType:library".into()]), tags_not: Some(targets), ..Default::default() },
        ));
    }
    if o.get("ignoredCircularDependencies").is_some_and(|v| *v != json!([])) {
        warnings.push(format!("{full}: ignoredCircularDependencies isn't converted"));
    }
    // checkNestedExternalImports compares the imported project's specifier
    // with the nested package's name, so it never reports anything (Nx 21).
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
        match c.get("onlyDependOnLibsWithTags").map(strings) {
            Some(only) if only.is_empty() => out.push(group_rule(
                full,
                sev,
                &format!("A project {label} cannot depend on any libs with tags."),
                from.clone(),
                // Any tag of its own (not tangle's projectType: / target: facts).
                ToSpec { tags: Some(vec!["/^(?!projectType:|target:)/".into()]), ..Default::default() },
            )),
            Some(only) if !only.iter().any(|t| t == "*") => out.push(group_rule(
                full,
                sev,
                &format!("A project {label} can only depend on libs tagged {}.", only.join(", ")),
                from.clone(),
                ToSpec { tags_not: Some(only), ..Default::default() },
            )),
            _ => {}
        }
        let not = strings(c.get("notDependOnLibsWithTags").unwrap_or(&Value::Null));
        if !not.is_empty() {
            // Transitively: nothing the target depends on may have them either.
            out.push(group_rule(
                full,
                sev,
                &format!("A project {label} cannot depend on libs tagged {}, directly or indirectly.", not.join(", ")),
                from.clone(),
                ToSpec { reaches_tags: Some(not), ..Default::default() },
            ));
        }
        // External imports: module rules on the project's files (they carry
        // its tags), matched against the specifier as Nx does.
        let spec_re = |globs: Vec<String>| Pat::any(&globs.iter().map(|g| nx_external(g)).collect::<Vec<_>>());
        let banned = strings(c.get("bannedExternalImports").unwrap_or(&Value::Null));
        if !banned.is_empty() {
            out.push(rule(
                full,
                sev,
                &format!("A project {label} cannot import {}.", banned.join(", ")),
                from.clone(),
                ToSpec { dependency_types: Some(NPM_TYPES.iter().map(|s| s.to_string()).collect()), specifier: Some(spec_re(banned)), ..Default::default() },
            ));
        }
        if let Some(allowed) = c.get("allowedExternalImports").map(strings) {
            out.push(rule(
                full,
                sev,
                &format!("A project {label} can only import external packages {}.", allowed.join(", ")),
                from.clone(),
                ToSpec {
                    dependency_types: Some(NPM_TYPES.iter().map(|s| s.to_string()).collect()),
                    specifier_not: (!allowed.is_empty()).then(|| spec_re(allowed)),
                    ..Default::default()
                },
            ));
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
    out.into_iter().map(finish).collect()
}

/// One pattern of a `boundaries/elements` descriptor: a regex over element
/// ids (the element's folder, or the file itself in file mode) whose
/// wildcards are capture groups, numbered as the plugin captures them.
#[derive(Clone)]
struct ElemPattern {
    parts: Vec<Part>,
    /// Captured value name → wildcard index (the last one wins, as a
    /// `capture` name beats the same `baseCapture` name).
    names: Vec<(String, usize)>,
    folder: bool,
}

impl ElemPattern {
    /// The id regex (unanchored), with wildcard `k` narrowed to `over[k]`.
    /// Every wildcard stays a capture group, so `$N` is wildcard `N - 1`.
    fn render(&self, over: &[(usize, String)]) -> String {
        let mut wild = 0;
        self.parts
            .iter()
            .map(|p| match p {
                Part::Lit(l) => l.clone(),
                Part::Wild(w) => {
                    let re = over.iter().find(|(k, _)| *k == wild).map_or(w.as_str(), |(_, r)| r.as_str());
                    wild += 1;
                    format!("({re})")
                }
            })
            .collect()
    }

    fn group(&self, name: &str) -> Option<usize> {
        self.names.iter().rev().find(|(n, _)| n == name).map(|(_, k)| *k)
    }
}

struct Element {
    ty: String,
    patterns: Vec<ElemPattern>,
}

/// `boundaries/elements` as patterns (captures, `basePattern`, modes).
fn boundary_elements(ctx: &Ctx) -> Vec<Element> {
    let mut out: Vec<Element> = vec![];
    for e in &ctx.elements {
        let Some(ty) = e.get("type").and_then(Value::as_str) else { continue };
        let mode = e.get("mode").and_then(Value::as_str).unwrap_or("folder");
        let partial = e.get("partialMatch") != Some(&json!(false));
        let base = e.get("basePattern").and_then(Value::as_str).filter(|_| partial);
        let (capture, base_capture) = (strings(e.get("capture").unwrap_or(&Value::Null)), strings(e.get("baseCapture").unwrap_or(&Value::Null)));
        let mut patterns = vec![];
        for p in strings(e.get("pattern").unwrap_or(&Value::Null)) {
            // Matched from the right (any leading folders) unless the whole
            // path must match; `basePattern` must match from the root.
            let mut parts = vec![];
            let mut names: Vec<(String, usize)> = vec![];
            if let Some(b) = base {
                parts = glob::parts(b.trim_end_matches('/'));
                parts.push(Part::Lit("/(?:.*/)?".into()));
                names.extend(base_capture.iter().cloned().zip(0..));
            } else if partial && mode != "full" {
                parts.push(Part::Lit("(?:.*/)?".into()));
            }
            let offset = parts.iter().filter(|p| matches!(p, Part::Wild(_))).count();
            parts.extend(glob::parts(p.trim_end_matches('/')));
            names.extend(capture.iter().cloned().zip(offset..));
            patterns.push(ElemPattern { parts, names, folder: !partial || !matches!(mode, "file" | "full") });
        }
        if patterns.is_empty() {
            continue;
        }
        match out.iter_mut().find(|x| x.ty == ty) {
            Some(x) => x.patterns.extend(patterns),
            None => out.push(Element { ty: ty.into(), patterns }),
        }
    }
    out
}

impl Ctx {
    fn anchor(&self, body: &str) -> String {
        if self.prefix.is_empty() { format!("^(?:{body})$") } else { format!("^{}/(?:{body})$", fancy_regex::escape(&self.prefix)) }
    }

    /// Files `boundaries/ignore` / `boundaries/include` leave out, as a regex.
    fn ignored(&self) -> Option<String> {
        let re = |globs: &[String]| globs.iter().map(|g| to_regex(g, Plain::Exact, false, &self.prefix)).collect::<Vec<_>>();
        let mut alts = re(&self.ignore);
        if !self.include.is_empty() {
            alts.push(format!("(?!(?:{}))", re(&self.include).join("|")));
        }
        (!alts.is_empty()).then(|| Pat::any(&alts).0)
    }

    /// Import kinds `boundaries/dependency-nodes` leaves unchecked.
    fn unchecked_kinds(&self) -> Option<Vec<String>> {
        let nodes = strings(self.nodes.as_ref()?);
        let kinds: Vec<String> = [("require", "require"), ("dynamic-import", "dynamic"), ("export", "reexport")]
            .into_iter()
            .filter(|(node, _)| !nodes.iter().any(|n| n == node))
            .map(|(_, ty)| ty.to_string())
            .collect();
        (!kinds.is_empty()).then_some(kinds)
    }
}

/// `boundaries/elements` → `[[groups]]` (one group type per element type).
/// Ignored files belong to no group, so no rule sees them.
fn boundary_groups(ctx: &Ctx, elements: &[Element]) -> Vec<GroupDef> {
    let skip = ctx.ignored().map(|i| format!("(?!{i})")).unwrap_or_default();
    elements
        .iter()
        .map(|e| {
            let alts: Vec<String> =
                e.patterns.iter().map(|p| format!("{}{}", p.render(&[]), if p.folder { "(?:/|$)" } else { "$" })).collect();
            let path = ctx.anchor(&alts.join("|"));
            GroupDef { name: e.ty.clone(), path: Pat(format!("^{skip}{}", path.trim_start_matches('^').trim_end_matches('$'))), tags: vec![] }
        })
        .collect()
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

/// Captured-value conditions (all must hold): key → micromatch patterns.
type Captured = Vec<(String, Vec<String>)>;

/// A selector: element types, plus conditions on captured values.
#[derive(Clone)]
struct Sel {
    types: TypeSel,
    captured: Captured,
}

fn captured_conditions(v: &Value) -> Result<Vec<Captured>, String> {
    let one = |o: &Value| -> Result<Captured, String> {
        o.as_object()
            .ok_or(format!("captured selector {o} isn't an object"))?
            .iter()
            .map(|(k, v)| match v {
                Value::String(_) | Value::Array(_) => Ok((k.clone(), strings(v))),
                other => Err(format!("captured value {other} isn't supported")),
            })
            .collect()
    };
    match v {
        // An array of objects: any of them.
        Value::Array(a) => a.iter().map(one).collect(),
        o => Ok(vec![one(o)?]),
    }
}

/// Parses a boundaries selector. Supports the legacy form (`"type"`,
/// `["type", { captured }]`, lists) and v6+ objects (`{ element: { type,
/// types, captured } }`, `{ to: … }`). `Err` = can't convert exactly.
fn selectors(v: &Value) -> Result<Vec<Sel>, String> {
    let one = |s: &str| {
        let (neg, pat) = s.strip_prefix('!').map_or((false, s), |p| (true, p));
        Sel { types: TypeSel { patterns: vec![pat.to_string()], negate: neg }, captured: vec![] }
    };
    match v {
        Value::Null => Ok(vec![]),
        Value::String(s) => Ok(vec![one(s)]),
        Value::Array(a) if a.len() == 2 && a[0].is_string() && a[1].is_object() => {
            let base = one(a[0].as_str().unwrap_or_default());
            Ok(captured_conditions(&a[1])?.into_iter().map(|captured| Sel { captured, ..base.clone() }).collect())
        }
        Value::Array(a) => a.iter().map(selectors).collect::<Result<Vec<_>, _>>().map(|v| v.concat()),
        Value::Object(o) => {
            if let Some(inner) = o.get("to").or_else(|| o.get("from")) {
                return selectors(inner);
            }
            let Some(el) = o.get("element") else {
                return Err(format!("selector {v} isn't about elements (files/modules aren't converted)"));
            };
            let extra: Vec<&String> =
                el.as_object().map(|e| e.keys().filter(|k| !["type", "types", "captured"].contains(&k.as_str())).collect()).unwrap_or_default();
            if !extra.is_empty() {
                return Err(format!("selector {v} uses {extra:?}"));
            }
            let types = if let Some(t) = el.get("type").and_then(Value::as_str) {
                one(t).types
            } else {
                match el.get("types") {
                    Some(Value::Object(t)) if t.contains_key("anyOf") => TypeSel { patterns: strings(&t["anyOf"]), negate: false },
                    Some(Value::Object(t)) if t.contains_key("noneOf") => TypeSel { patterns: strings(&t["noneOf"]), negate: true },
                    Some(other) => TypeSel { patterns: strings(other), negate: false },
                    None => TypeSel::any(),
                }
            };
            match el.get("captured") {
                None => Ok(vec![Sel { types, captured: vec![] }]),
                Some(c) => Ok(captured_conditions(c)?.into_iter().map(|captured| Sel { types: types.clone(), captured }).collect()),
            }
        }
        _ => Err(format!("selector {v} isn't supported")),
    }
}

/// A condition on one side of a dependency, as a regex on element ids.
#[derive(Clone, PartialEq)]
enum Cond {
    True,
    Never,
    Re(String),
}

/// A captured-value pattern as a regex. Templates (`{{ from.element.captured.x }}`,
/// `{{ from.x }}`, legacy `${from.x}`) may only refer to the source's
/// captures, from a condition on the target: they become `$N` backreferences.
fn value_regex(value: &str, on_target: bool, from: &ElemPattern) -> Result<Option<String>, String> {
    let template = regex::Regex::new(r"\{\{\s*([^}]+?)\s*\}\}|\$\{([^}]+)\}").expect("valid");
    let mut out = String::new();
    let mut last = 0;
    for c in template.captures_iter(value) {
        let m = c.get(0).expect("match");
        out.push_str(&glob::body(&value[last..m.start()]));
        last = m.end();
        let expr = c.get(1).or_else(|| c.get(2)).expect("group").as_str().trim();
        let key = ["from.element.captured.", "from.captured.", "from."].iter().find_map(|p| expr.strip_prefix(p));
        let (Some(key), true) = (key, on_target) else {
            return Err(format!("template {} isn't converted (only a target's condition on the source's captured values is)", m.as_str()));
        };
        match from.group(key) {
            Some(k) => out.push_str(&format!("${}", k + 1)),
            // An unknown value renders empty, which matches nothing.
            None => return Ok(None),
        }
    }
    out.push_str(&glob::body(&value[last..]));
    Ok(Some(out))
}

/// Captured-value conditions on an element pattern. `from` is the source's
/// pattern, for templates in conditions on the target.
fn cond(ctx: &Ctx, p: &ElemPattern, captured: &Captured, from: Option<&ElemPattern>) -> Result<Cond, String> {
    if captured.is_empty() {
        return Ok(Cond::True);
    }
    let mut over = vec![];
    for (key, values) in captured {
        let Some(k) = p.group(key) else { return Ok(Cond::Never) };
        let mut alts = vec![];
        for v in values {
            match from {
                Some(f) => alts.extend(value_regex(v, true, f)?),
                None if v.contains("{{") || v.contains("${") => return Err(format!("template in {v:?} isn't converted")),
                None => alts.push(glob::body(v)),
            }
        }
        if alts.is_empty() {
            return Ok(Cond::Never);
        }
        over.push((k, alts.join("|")));
    }
    Ok(Cond::Re(ctx.anchor(&p.render(&over))))
}

/// Several target patterns: any of them.
fn cond_any(conds: Vec<Cond>) -> Cond {
    if conds.contains(&Cond::True) {
        return Cond::True;
    }
    let res: Vec<String> = conds.into_iter().filter_map(|c| if let Cond::Re(r) = c { Some(r) } else { None }).collect();
    if res.is_empty() { Cond::Never } else { Cond::Re(Pat::any(&res).0) }
}

/// One policy effect (allow or disallow) as it applies to a pair of element
/// types: alternatives of (source, target) captured conditions.
struct Effect {
    allow: bool,
    alts: Vec<(Captured, Captured)>,
    message: Option<String>,
}

/// Replays last-match-wins policies with captured-value conditions, as
/// rules for the source pattern `fp`. An import is reported when a holding
/// `disallow` isn't followed by a holding `allow`, or (default disallow)
/// when no `allow` holds. A later `allow`'s condition must be false: on the
/// source *or* the target side, so each such clause may double the rules.
#[allow(clippy::too_many_arguments)]
fn conditional_rules(
    ctx: &Ctx,
    full: &str,
    sev: Severity,
    effects: &[Effect],
    default_allow: bool,
    from: (&str, &ElemPattern),
    to: &Element,
    fallback: &str,
    warnings: &mut Vec<String>,
) -> Vec<Rule> {
    let (fty, fp) = from;
    let side = |f: &Captured, t: &Captured| -> Result<(Cond, Cond), String> {
        let fc = cond(ctx, fp, f, None)?;
        let tc = cond_any(to.patterns.iter().map(|tp| cond(ctx, tp, t, Some(fp))).collect::<Result<_, _>>()?);
        Ok((fc, tc))
    };
    let mut out = vec![];
    let mut terms: Vec<(Option<&Effect>, Captured, Captured, usize)> = vec![];
    for (i, e) in effects.iter().enumerate().filter(|(_, e)| !e.allow) {
        for (f, t) in &e.alts {
            terms.push((Some(e), f.clone(), t.clone(), i + 1));
        }
    }
    if !default_allow {
        terms.push((None, vec![], vec![], 0));
    }
    'terms: for (effect, f, t, later) in terms {
        let (fpos, tpos) = match side(&f, &t) {
            Ok(c) => c,
            Err(why) => {
                warnings.push(format!("{full}: {why}"));
                continue;
            }
        };
        if fpos == Cond::Never || tpos == Cond::Never {
            continue;
        }
        // Clauses: no later allow may hold.
        let mut forced_f: Vec<String> = vec![];
        let mut forced_t: Vec<String> = vec![];
        let mut either: Vec<(String, String)> = vec![];
        for e in effects[later..].iter().filter(|e| e.allow) {
            for (af, at) in &e.alts {
                match side(af, at) {
                    Err(why) => {
                        warnings.push(format!("{full}: {why}"));
                        continue 'terms;
                    }
                    Ok((Cond::Never, _)) | Ok((_, Cond::Never)) => {}
                    Ok((Cond::True, Cond::True)) => continue 'terms,
                    Ok((Cond::True, Cond::Re(r))) => forced_t.push(r),
                    Ok((Cond::Re(r), Cond::True)) => forced_f.push(r),
                    Ok((Cond::Re(a), Cond::Re(b))) => either.push((a, b)),
                }
            }
        }
        if either.len() > 6 {
            warnings.push(format!("{full}: too many captured-value conditions to convert for {fty} → {}", to.ty));
            continue;
        }
        for choice in 0..1u32 << either.len() {
            let (mut nf, mut nt) = (forced_f.clone(), forced_t.clone());
            for (bit, (a, b)) in either.iter().enumerate() {
                if choice >> bit & 1 == 0 { nf.push(a.clone()) } else { nt.push(b.clone()) }
            }
            let re = |c: &Cond| if let Cond::Re(r) = c { Some(Pat(r.clone())) } else { None };
            out.push(group_rule(
                full,
                sev,
                effect.and_then(|e| e.message.as_deref()).unwrap_or(fallback),
                FromSpec {
                    tags: Some(vec![fty.into()]),
                    // Always the source pattern, so `$N` refer to its captures.
                    path: re(&fpos).or_else(|| Some(Pat(ctx.anchor(&fp.render(&[]))))),
                    path_not: (!nf.is_empty()).then(|| Pat::any(&nf)),
                    ..Default::default()
                },
                ToSpec { tags: Some(vec![to.ty.clone()]), path: re(&tpos), path_not: (!nt.is_empty()).then(|| Pat::any(&nt)), ..Default::default() },
            ));
        }
    }
    out
}

/// A `boundaries/dependencies` policy.
struct Policy {
    from: Vec<Sel>,
    allow: Vec<Sel>,
    disallow: Vec<Sel>,
    message: Option<String>,
}

/// Parses policies' selectors; unconvertible ones are skipped with a warning.
fn parse_policies(full: &str, o: &Value, warnings: &mut Vec<String>) -> Vec<Policy> {
    let policies = o.get("policies").or_else(|| o.get("rules")).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = vec![];
    for (i, p) in policies.iter().enumerate() {
        if p.get("importKind").is_some() {
            warnings.push(format!("{full}: importKind isn't converted (the policy applies to all imports)"));
        }
        let parsed = (|| -> Result<_, String> {
            let from = match p.get("from") {
                None => vec![Sel { types: TypeSel::any(), captured: vec![] }],
                Some(f) => selectors(f)?,
            };
            Ok((from, selectors(p.get("allow").unwrap_or(&Value::Null))?, selectors(p.get("disallow").unwrap_or(&Value::Null))?))
        })();
        match parsed {
            Ok((from, allow, disallow)) => {
                out.push(Policy { from, allow, disallow, message: p.get("message").and_then(Value::as_str).map(String::from) })
            }
            Err(why) => warnings.push(format!("{full}: policy #{} skipped — {why}", i + 1)),
        }
    }
    out
}

/// `boundaries/dependencies` (v6+ `policies`) and `boundaries/element-types`
/// (legacy `rules`): replays the policies in order — the last matching one
/// wins, a `disallow` beats an `allow` in the same policy, and without a
/// match the import is disallowed unless `default` is "allow".
fn convert_boundaries(full: &str, sev: Severity, o: &Value, ctx: &Ctx, elements: &[Element], warnings: &mut Vec<String>) -> Vec<Rule> {
    if elements.is_empty() {
        warnings.push(format!("{full}: no boundaries/elements settings found"));
        return vec![];
    }
    let default_allow = o.get("default").and_then(Value::as_str) == Some("allow");
    let compiled = parse_policies(full, o, warnings);
    let mut out = vec![];
    for from in elements {
        let mut denied: Vec<String> = vec![];
        let mut message = None;
        for to in elements {
            // The policy effects that can apply to this pair, in order.
            let mut effects: Vec<Effect> = vec![];
            for p in &compiled {
                for (is_allow, sel) in [(true, &p.allow), (false, &p.disallow)] {
                    let alts: Vec<(Captured, Captured)> = p
                        .from
                        .iter()
                        .filter(|f| f.types.matches(&from.ty))
                        .flat_map(|f| sel.iter().filter(|t| t.types.matches(&to.ty)).map(|t| (f.captured.clone(), t.captured.clone())))
                        .collect();
                    if !alts.is_empty() {
                        effects.push(Effect { allow: is_allow, alts, message: if is_allow { None } else { p.message.clone() } });
                    }
                }
            }
            let fallback = format!("Elements of type \"{}\" can't import elements of type \"{}\".", from.ty, to.ty);
            if effects.iter().any(|e| e.alts.iter().any(|(f, t)| !f.is_empty() || !t.is_empty())) {
                for fp in &from.patterns {
                    out.extend(conditional_rules(ctx, full, sev, &effects, default_allow, (&from.ty, fp), to, &fallback, warnings));
                }
                continue;
            }
            // No conditions: a plain verdict for the pair.
            let mut allowed = default_allow;
            let mut msg = None;
            for e in &effects {
                allowed = e.allow;
                msg = e.message.clone();
            }
            if !allowed {
                denied.push(to.ty.clone());
                message = message.or(msg);
            }
        }
        if !denied.is_empty() {
            let comment = message.unwrap_or_else(|| format!("Elements of type \"{}\" can't import {}.", from.ty, denied.join(", ")));
            out.push(group_rule(
                full,
                sev,
                &comment,
                FromSpec { tags: Some(vec![from.ty.clone()]), ..Default::default() },
                ToSpec { tags: Some(denied), ..Default::default() },
            ));
        }
    }
    out
}

/// Glob lists from an entry-point / external policy effect.
fn effect_globs(v: Option<&Value>) -> Vec<Value> {
    match v {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(a)) if a.len() == 2 && a[0].is_string() && a[1].is_object() => vec![Value::Array(a.clone())],
        Some(Value::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
    }
}

/// Last-match-wins over one-sided (target) conditions: rules with
/// `to_re(disallowed)` and `to_not(later allowed)`. `effects` are
/// (allow, regexes, message) in order.
fn one_sided(effects: &[(bool, Vec<String>, Option<String>)], default_allow: bool) -> Vec<(Option<Pat>, Option<Pat>, Option<String>)> {
    let mut out = vec![];
    let allows_after = |i: usize| -> Vec<String> { effects[i..].iter().filter(|e| e.0).flat_map(|e| e.1.clone()).collect() };
    for (i, (allow, res, msg)) in effects.iter().enumerate() {
        if !allow && !res.is_empty() {
            let later = allows_after(i + 1);
            out.push((Some(Pat::any(res)), (!later.is_empty()).then(|| Pat::any(&later)), msg.clone()));
        }
    }
    if !default_allow {
        let all = allows_after(0);
        out.push((None, (!all.is_empty()).then(|| Pat::any(&all)), None));
    }
    out
}

/// `boundaries/entry-point`: other elements may import an element only
/// through the files its policies allow (paths inside the element).
fn convert_entry_point(full: &str, sev: Severity, o: &Value, ctx: &Ctx, elements: &[Element], warnings: &mut Vec<String>) -> Vec<Rule> {
    let default_allow = o.get("default").and_then(Value::as_str) == Some("allow");
    let policies = parse_policies_raw(o);
    let all_types: Vec<String> = elements.iter().map(|e| e.ty.clone()).collect();
    let mut out = vec![];
    for el in elements {
        if el.patterns.iter().any(|p| !p.folder) {
            warnings.push(format!("{full}: element type \"{}\" isn't a folder element, so its entry points aren't converted", el.ty));
            continue;
        }
        let root = el.patterns.iter().map(|p| p.render(&[])).collect::<Vec<_>>().join("|");
        let file = |globs: &[Value]| -> Result<Vec<String>, String> {
            globs
                .iter()
                .map(|g| match g.as_str() {
                    Some(g) if !g.contains("${") && !g.contains("{{") => Ok(ctx.anchor(&format!("(?:{root})/(?:{})", glob::body(g)))),
                    _ => Err(format!("entry {g} isn't converted")),
                })
                .collect()
        };
        let mut effects = vec![];
        for (i, p) in policies.iter().enumerate() {
            let targets = match selectors(p.get("target").or_else(|| p.get("to")).unwrap_or(&Value::Null)) {
                Ok(t) => t,
                Err(why) => {
                    warnings.push(format!("{full}: policy #{} skipped — {why}", i + 1));
                    continue;
                }
            };
            if !targets.iter().any(|t| t.types.matches(&el.ty)) {
                continue;
            }
            if targets.iter().any(|t| !t.captured.is_empty()) {
                warnings.push(format!("{full}: policy #{} uses captured values, which entry-point conversion ignores", i + 1));
            }
            for (allow, key) in [(true, "allow"), (false, "disallow")] {
                match file(&effect_globs(p.get(key))) {
                    Ok(res) if !res.is_empty() => effects.push((allow, res, p.get("message").and_then(Value::as_str).map(String::from))),
                    Ok(_) => {}
                    Err(why) => warnings.push(format!("{full}: policy #{} skipped — {why}", i + 1)),
                }
            }
        }
        for (path, path_not, msg) in one_sided(&effects, default_allow) {
            out.push(rule(
                full,
                sev,
                msg.as_deref().unwrap_or(&format!("Elements of type \"{}\" must be imported through their entry point.", el.ty)),
                FromSpec { tags: Some(all_types.clone()), ..Default::default() },
                ToSpec { tags: Some(vec![el.ty.clone()]), cross_group: Some(true), path, path_not, ..Default::default() },
            ));
        }
    }
    out
}

fn parse_policies_raw(o: &Value) -> Vec<Value> {
    o.get("policies").or_else(|| o.get("rules")).and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `boundaries/external`: which external and core modules each element
/// type may import (module name globs, optionally with a path inside it).
fn convert_external(full: &str, sev: Severity, o: &Value, elements: &[Element], warnings: &mut Vec<String>) -> Vec<Rule> {
    let default_allow = o.get("default").and_then(Value::as_str) == Some("allow");
    let policies = parse_policies_raw(o);
    let module = |m: &Value| -> Result<String, String> {
        let (name, opts) = match m {
            Value::String(s) => (s.as_str(), None),
            Value::Array(a) => (a[0].as_str().unwrap_or_default(), a.get(1)),
            _ => return Err(format!("module selector {m} isn't supported")),
        };
        if opts.and_then(|o| o.get("specifiers")).is_some() {
            return Err(format!("{m}: `specifiers` (imported names) aren't converted"));
        }
        let paths = opts.and_then(|o| o.get("path")).map(strings).unwrap_or_default();
        let module = glob::body(name);
        Ok(if paths.is_empty() {
            format!("^{module}(?:/.*)?$")
        } else {
            let inner: Vec<String> = paths.iter().map(|p| glob::body(p.trim_start_matches('/'))).collect();
            format!("^{module}/(?:{})$", inner.join("|"))
        })
    };
    let mut kinds: Vec<String> = NPM_TYPES.iter().map(|s| s.to_string()).collect();
    kinds.push("core".into());
    let mut out = vec![];
    for el in elements {
        let mut effects = vec![];
        for (i, p) in policies.iter().enumerate() {
            let from = match p.get("from").map(selectors).unwrap_or(Ok(vec![Sel { types: TypeSel::any(), captured: vec![] }])) {
                Ok(f) => f,
                Err(why) => {
                    warnings.push(format!("{full}: policy #{} skipped — {why}", i + 1));
                    continue;
                }
            };
            if !from.iter().any(|f| f.types.matches(&el.ty)) {
                continue;
            }
            for (allow, key) in [(true, "allow"), (false, "disallow")] {
                match effect_globs(p.get(key)).iter().map(module).collect::<Result<Vec<_>, _>>() {
                    Ok(res) if !res.is_empty() => effects.push((allow, res, p.get("message").and_then(Value::as_str).map(String::from))),
                    Ok(_) => {}
                    Err(why) => warnings.push(format!("{full}: policy #{} skipped — {why}", i + 1)),
                }
            }
        }
        for (specifier, specifier_not, msg) in one_sided(&effects, default_allow) {
            out.push(rule(
                full,
                sev,
                msg.as_deref().unwrap_or(&format!("Elements of type \"{}\" can't import this module.", el.ty)),
                FromSpec { tags: Some(vec![el.ty.clone()]), ..Default::default() },
                ToSpec { dependency_types: Some(kinds.clone()), specifier, specifier_not, ..Default::default() },
            ));
        }
    }
    out
}

/// `boundaries/no-unknown(-dependencies)`: elements may not import local
/// files that belong to no element (ignored files excepted).
fn convert_no_unknown(full: &str, sev: Severity, o: &Value, ctx: &Ctx, elements: &[Element], warnings: &mut Vec<String>) -> Vec<Rule> {
    if o.get("require").and_then(Value::as_str).is_some_and(|r| r == "file" || r == "all") {
        warnings.push(format!("{full}: require = {} (file descriptors) isn't converted", o["require"]));
        return vec![];
    }
    let types: Vec<String> = elements.iter().map(|e| e.ty.clone()).collect();
    vec![rule(
        full,
        sev,
        "Dependencies to unknown elements and files are not allowed.",
        FromSpec { tags: Some(types.clone()), ..Default::default() },
        ToSpec {
            dependency_types: Some(vec!["local".into()]),
            tags_not: Some(types),
            path_not: ctx.ignored().map(Pat),
            ..Default::default()
        },
    )]
}

fn convert(full: &str, short: &str, sev: Severity, opts: &[Value], ctx: &Ctx, warnings: &mut Vec<String>) -> Vec<Rule> {
    let prefix = ctx.prefix.as_str();
    let o = opts.first().cloned().unwrap_or(json!({}));
    match short {
        "enforce-module-boundaries" => convert_nx(full, sev, &o, warnings),
        _ if full.starts_with("boundaries/") => {
            let els = &ctx.parsed;
            let mut rules = match short {
                "element-types" | "dependencies" => convert_boundaries(full, sev, &o, ctx, els, warnings),
                "entry-point" => convert_entry_point(full, sev, &o, ctx, els, warnings),
                "external" => convert_external(full, sev, &o, els, warnings),
                _ => convert_no_unknown(full, sev, &o, ctx, els, warnings),
            };
            // Import kinds the plugin was told not to check.
            if let Some(kinds) = ctx.unchecked_kinds() {
                for r in &mut rules {
                    r.to.dependency_types_not.get_or_insert_with(Vec::new).extend(kinds.clone());
                }
            }
            rules
        }
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
