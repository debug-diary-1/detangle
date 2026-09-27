//! Import aliases from webpack (`resolve.alias`, `resolve.modules`,
//! `resolve.extensions`), Babel (`babel-plugin-module-resolver`) and tangle's
//! own `[options.aliases]`. They rewrite a specifier *before* resolution, so
//! the rest of the pipeline (tsconfig paths, package exports, …) still applies.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use serde::Deserialize;

use crate::config::Options;

#[derive(Debug)]
enum Key {
    /// Matches the key itself or anything below it (`key/...`).
    Prefix(String),
    /// Webpack's `name$`: only the exact specifier.
    Exact(String),
    /// Babel's `^regex` keys; the value may use `\1` (or `$1`).
    Regex(Regex),
}

#[derive(Debug)]
struct Rule {
    key: Key,
    /// Candidate replacements, tried in order. `None` = ignore the import
    /// (webpack's `false`).
    targets: Vec<Option<String>>,
    /// Directory that relative targets are relative to.
    base: PathBuf,
}

/// What an alias turned a specifier into.
pub enum Rewrite {
    /// Try these specifiers (absolute paths or module names), in order.
    Candidates(Vec<String>),
    /// The import is deliberately ignored.
    Ignore,
}

#[derive(Debug, Default)]
pub struct Aliases {
    rules: Vec<Rule>,
    /// Babel `root`: directories that bare specifiers are also looked up in.
    pub roots: Vec<PathBuf>,
    /// Extra module directories and extensions from webpack.
    pub modules: Vec<String>,
    pub extensions: Vec<String>,
}

impl Aliases {
    pub fn load(root: &Path, opts: &Options) -> Result<Self> {
        let mut a = Aliases::default();
        // Longest keys first, so `@app/ui` wins over `@app`.
        let mut native: Vec<(&String, &String)> = opts.aliases.iter().collect();
        native.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        for (k, v) in native {
            a.rules.push(Rule { key: key_for(k)?, targets: vec![Some(v.clone())], base: root.to_path_buf() });
        }
        if let Some(file) = &opts.webpack_config {
            a.add_webpack(&root.join(file)).with_context(|| format!("loading webpack config {file}"))?;
        }
        if let Some(file) = &opts.babel_config {
            a.add_babel(&root.join(file)).with_context(|| format!("loading babel config {file}"))?;
        }
        Ok(a)
    }

    /// Applies the first matching alias.
    pub fn rewrite(&self, spec: &str) -> Option<Rewrite> {
        for rule in &self.rules {
            let replaced: Vec<Option<String>> = match &rule.key {
                Key::Exact(k) if spec == k => rule.targets.clone(),
                Key::Prefix(k) if spec == k => rule.targets.clone(),
                Key::Prefix(k) if spec.starts_with(k.as_str()) && spec.as_bytes()[k.len()] == b'/' => {
                    let rest = &spec[k.len()..];
                    rule.targets.iter().map(|t| t.as_ref().map(|t| format!("{}{rest}", t.trim_end_matches('/')))).collect()
                }
                Key::Regex(re) => match re.captures(spec).ok().flatten() {
                    Some(caps) => rule
                        .targets
                        .iter()
                        .map(|t| {
                            t.as_ref().map(|t| {
                                let mut out = t.clone();
                                for i in (1..caps.len()).rev() {
                                    let v = caps.get(i).map_or("", |m| m.as_str());
                                    out = out.replace(&format!("\\{i}"), v).replace(&format!("${i}"), v);
                                }
                                out
                            })
                        })
                        .collect(),
                    None => continue,
                },
                _ => continue,
            };
            if replaced.iter().all(Option::is_none) {
                return Some(Rewrite::Ignore);
            }
            let candidates = replaced
                .into_iter()
                .flatten()
                .map(|t| {
                    if t.starts_with("./") || t.starts_with("../") || t == "." {
                        normalize(&rule.base.join(&t)).to_string_lossy().into_owned()
                    } else {
                        t
                    }
                })
                .collect();
            return Some(Rewrite::Candidates(candidates));
        }
        None
    }

    fn add_webpack(&mut self, file: &Path) -> Result<()> {
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            alias: Value,
            #[serde(default, rename = "onlyModule")]
            only_module: bool,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Value {
            Paths(Vec<Value>),
            Path(String),
            /// `false`: ignore the module.
            Ignore(serde::de::IgnoredAny),
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Resolve {
            alias: Vec<Entry>,
            modules: Vec<String>,
            extensions: Vec<String>,
        }
        let base = file.parent().unwrap_or(Path::new(".")).to_path_buf();
        let r: Resolve = serde_json::from_str(&run_node(WEBPACK_LOADER, file)?)?;
        fn flatten(v: Value, out: &mut Vec<Option<String>>) {
            match v {
                Value::Path(p) => out.push(Some(p)),
                Value::Ignore(_) => out.push(None),
                Value::Paths(list) => list.into_iter().for_each(|v| flatten(v, out)),
            }
        }
        for e in r.alias {
            let mut targets = vec![];
            flatten(e.alias, &mut targets);
            let key = match e.name.strip_suffix('$') {
                Some(exact) => Key::Exact(exact.to_string()),
                None if e.only_module => Key::Exact(e.name),
                None => Key::Prefix(e.name),
            };
            self.rules.push(Rule { key, targets, base: base.clone() });
        }
        for m in r.modules {
            let m = if Path::new(&m).is_absolute() || !m.contains('/') { m } else { normalize(&base.join(&m)).to_string_lossy().into_owned() };
            if m != "node_modules" && !self.modules.contains(&m) {
                self.modules.push(m);
            }
        }
        self.extensions.extend(r.extensions);
        Ok(())
    }

    fn add_babel(&mut self, file: &Path) -> Result<()> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Resolver {
            root: Vec<String>,
            alias: Vec<(String, String)>,
            cwd: Option<String>,
        }
        let found: Option<Resolver> = serde_json::from_str(&run_node(BABEL_LOADER, file)?)?;
        let Some(r) = found else {
            bail!("no babel-plugin-module-resolver entry in {}", file.display());
        };
        let dir = file.parent().unwrap_or(Path::new(".")).to_path_buf();
        // `cwd: "babelrc" | "packagejson"` mean "the config's own directory" here.
        let base = match r.cwd.as_deref() {
            Some(c) if c != "babelrc" && c != "packagejson" => dir.join(c),
            _ => dir,
        };
        for (k, v) in r.alias {
            self.rules.push(Rule { key: key_for(&k)?, targets: vec![Some(v)], base: base.clone() });
        }
        self.roots.extend(r.root.into_iter().map(|p| base.join(p)));
        Ok(())
    }
}

/// Resolves `.` and `..` segments lexically.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn key_for(k: &str) -> Result<Key> {
    Ok(if k.starts_with('^') {
        Key::Regex(Regex::new(k).with_context(|| format!("alias {k:?} isn't a valid regex"))?)
    } else {
        Key::Prefix(k.to_string())
    })
}

fn run_node(script: &str, file: &Path) -> Result<String> {
    let abs = std::fs::canonicalize(file).with_context(|| format!("{} not found", file.display()))?;
    let out = Command::new("node")
        .args(["-e", script])
        .env("TANGLE_CONFIG_FILE", &abs)
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

/// Evaluates a webpack config (object, array, function or promise) and
/// prints the parts of `resolve` tangle uses, with aliases normalised to
/// webpack's array form.
const WEBPACK_LOADER: &str = r#"
const { pathToFileURL } = require('url');
(async () => {
  const file = process.env.TANGLE_CONFIG_FILE;
  const m = await import(pathToFileURL(file).href);
  let c = m.default ?? m;
  if (typeof c === 'function') c = await c({}, { mode: 'development', env: {} });
  const configs = Array.isArray(c) ? c : [c];
  const out = { alias: [], modules: [], extensions: [] };
  for (const cfg of configs) {
    const r = (cfg && cfg.resolve) || {};
    const a = r.alias || {};
    if (Array.isArray(a)) out.alias.push(...a.map(({ name, alias, onlyModule }) => ({ name, alias, onlyModule: !!onlyModule })));
    else for (const [name, alias] of Object.entries(a)) out.alias.push({ name, alias, onlyModule: false });
    for (const x of r.modules || []) if (!out.modules.includes(x)) out.modules.push(x);
    for (const x of r.extensions || []) if (x !== '...' && !out.extensions.includes(x)) out.extensions.push(x);
  }
  process.stdout.write(JSON.stringify(out));
})().catch((e) => { console.error((e && e.message) || String(e)); process.exit(1); });
"#;

/// Evaluates a Babel config (.babelrc / babel.config.js / package.json) with
/// a stub `api`, and prints babel-plugin-module-resolver's options (alias
/// keys in order, as [key, value] pairs), or null.
const BABEL_LOADER: &str = r#"
const fs = require('fs'), path = require('path'), { pathToFileURL } = require('url');
(async () => {
  const file = process.env.TANGLE_CONFIG_FILE;
  let c;
  if (/\.(c|m)?js$/.test(file)) {
    const m = await import(pathToFileURL(file).href);
    c = m.default ?? m;
  } else {
    const text = fs.readFileSync(file, 'utf8');
    try { c = JSON.parse(text); }
    catch { c = JSON.parse(text.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '').replace(/,(\s*[}\]])/g, '$1')); }
    if (path.basename(file) === 'package.json') c = c.babel || {};
  }
  if (typeof c === 'function') {
    const cache = Object.assign(() => {}, { forever() {}, never() {}, using: (f) => f(), invalidate: (f) => f() });
    c = c({ cache, env: (x) => (x === undefined ? 'development' : typeof x === 'function' ? x('development') : [].concat(x).includes('development')),
            caller: () => undefined, version: '7.0.0', assertVersion() {} });
  }
  let found = null;
  for (const p of (c && c.plugins) || []) {
    const [name, opts] = Array.isArray(p) ? p : [p, {}];
    if (typeof name === 'string' && /(^|\/)(babel-plugin-)?module-resolver$/.test(name)) found = opts || {};
  }
  if (found) found = { root: [].concat(found.root || []), alias: Object.entries(found.alias || {}), cwd: found.cwd };
  process.stdout.write(JSON.stringify(found));
})().catch((e) => { console.error((e && e.message) || String(e)); process.exit(1); });
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(list: &[(&str, &[Option<&str>])]) -> Aliases {
        Aliases {
            rules: list
                .iter()
                .map(|(k, t)| Rule {
                    key: match k.strip_suffix('$') {
                        Some(e) if !k.starts_with('^') => Key::Exact(e.into()),
                        _ => key_for(k).unwrap(),
                    },
                    targets: t.iter().map(|x| x.map(String::from)).collect(),
                    base: PathBuf::from("/p"),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn cands(a: &Aliases, spec: &str) -> Option<Vec<String>> {
        match a.rewrite(spec)? {
            Rewrite::Candidates(c) => Some(c),
            Rewrite::Ignore => Some(vec!["<ignored>".into()]),
        }
    }

    #[test]
    fn webpack_and_babel_semantics() {
        let a = rules(&[
            ("utils$", &[Some("/p/src/utils/index.js")]),
            ("@c", &[Some("/p/src/components"), Some("/p/src/fallback")]),
            ("legacy", &[None]),
            ("^@feature/(.+)$", &[Some("./src/features/\\1")]),
            ("~", &[Some("./src")]),
        ]);
        assert_eq!(cands(&a, "utils").unwrap(), ["/p/src/utils/index.js"]);
        assert_eq!(cands(&a, "utils/x"), None); // `$` = exact only
        assert_eq!(cands(&a, "@c/Button").unwrap(), ["/p/src/components/Button", "/p/src/fallback/Button"]);
        assert_eq!(cands(&a, "@cx"), None); // prefix must end at a path boundary
        assert_eq!(cands(&a, "legacy").unwrap(), ["<ignored>"]);
        assert_eq!(cands(&a, "@feature/cart").unwrap(), ["/p/src/features/cart"]);
        assert_eq!(cands(&a, "~/a/b").unwrap(), ["/p/src/a/b"]);
        assert_eq!(cands(&a, "react"), None);
    }
}
