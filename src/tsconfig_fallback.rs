//! A tsconfig that can't be loaded as a whole, because an `extends` entry
//! names a package that isn't installed (or a file that doesn't exist).
//! The resolver then fails for every file it governs, and resolution falls
//! back to ignoring tsconfigs, which would also drop the file's own `paths`
//! and `baseUrl`. TypeScript reports the missing base and still applies
//! them; this keeps them too.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::stamps::resolve_extends;

/// `paths` patterns with their targets, in the tsconfig's order.
type Paths = Vec<(String, Vec<String>)>;

pub struct Fallback {
    /// The tsconfig that failed to load.
    pub tsconfig: PathBuf,
    /// The `extends` entries that can't be found, with the tsconfig naming
    /// each: in its own chain, or in that of a project it `references`.
    pub missing: Vec<(PathBuf, String)>,
    /// `paths` patterns and their targets, which resolve against `paths_base`.
    paths: Paths,
    paths_base: PathBuf,
    base_url: Option<PathBuf>,
}

/// `paths` (with the directory they're relative to) and `baseUrl` (absolute).
#[derive(Default)]
struct Found {
    paths: Option<(Paths, PathBuf)>,
    base_url: Option<PathBuf>,
}

fn read(path: &Path) -> Option<Value> {
    let mut text = std::fs::read_to_string(path).ok()?;
    json_strip_comments::strip(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

fn extends_of(json: &Value) -> Vec<&str> {
    match json.get("extends") {
        Some(Value::String(s)) => vec![s],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

/// The tsconfig's own options, else the ones it inherits from the bases that
/// do load (a later `extends` entry overrides an earlier one).
fn options(path: &Path, depth: usize) -> Found {
    let Some(json) = read(path) else { return Found::default() };
    let dir = path.parent().unwrap_or(Path::new("."));
    let co = json.get("compilerOptions");
    let mut found = Found {
        paths: co.and_then(|c| c.get("paths")).and_then(Value::as_object).map(|o| {
            let paths = o
                .iter()
                .map(|(k, v)| (k.clone(), v.as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default()))
                .collect();
            (paths, dir.to_path_buf())
        }),
        base_url: co.and_then(|c| c.get("baseUrl")).and_then(Value::as_str).map(|b| dir.join(b)),
    };
    if depth < 8 {
        for base in extends_of(&json).into_iter().rev().filter_map(|s| resolve_extends(dir, s)).filter(|p| p.is_file()) {
            if found.paths.is_some() && found.base_url.is_some() {
                break;
            }
            let inherited = options(&base, depth + 1);
            found.paths = found.paths.or(inherited.paths);
            found.base_url = found.base_url.or(inherited.base_url);
        }
    }
    found
}

/// The `extends` entries that can't be found, reached from `path` through
/// `extends` and `references` (a solution-style tsconfig owns no files and
/// hands each to a referenced project).
fn missing(path: &Path, depth: usize, seen: &mut Vec<PathBuf>, out: &mut Vec<(PathBuf, String)>) {
    if depth > 8 || seen.iter().any(|p| p == path) {
        return;
    }
    seen.push(path.to_path_buf());
    let Some(json) = read(path) else { return };
    let dir = path.parent().unwrap_or(Path::new("."));
    for spec in extends_of(&json) {
        match resolve_extends(dir, spec).filter(|p| p.is_file()) {
            Some(base) => missing(&base, depth + 1, seen, out),
            None => out.push((path.to_path_buf(), spec.to_string())),
        }
    }
    let references = json.get("references").and_then(Value::as_array).into_iter().flatten();
    for r in references.filter_map(|r| r.get("path")).filter_map(Value::as_str) {
        let p = dir.join(r);
        let p = if p.is_dir() { p.join("tsconfig.json") } else { p };
        if p.is_file() {
            missing(&p, depth + 1, seen, out);
        }
    }
}

impl Fallback {
    /// `None` when the tsconfig can't be read, or nothing is missing from it.
    pub fn load(tsconfig: &Path) -> Option<Fallback> {
        let mut missing_bases = vec![];
        missing(tsconfig, 0, &mut vec![], &mut missing_bases);
        if missing_bases.is_empty() {
            return None;
        }
        let missing = missing_bases;
        let found = options(tsconfig, 0);
        let (paths, defined_in) = found.paths.unwrap_or_default();
        // As in TypeScript: `paths` are relative to `baseUrl` when there is
        // one, else to the tsconfig that declares them.
        let paths_base = found.base_url.clone().unwrap_or(defined_in);
        Some(Fallback { tsconfig: tsconfig.to_path_buf(), missing, paths, paths_base, base_url: found.base_url })
    }

    /// Where a bare `spec` may live, in the order TypeScript tries: the
    /// targets of the best-matching `paths` pattern (an exact pattern, else
    /// the longest prefix before `*`), then `baseUrl`.
    pub fn candidates(&self, spec: &str) -> Vec<PathBuf> {
        if spec.starts_with('.') || Path::new(spec).is_absolute() {
            return vec![];
        }
        let mut best: Option<(usize, &[String], &str)> = None;
        for (pat, targets) in &self.paths {
            match pat.split_once('*') {
                None if pat == spec => {
                    best = Some((usize::MAX, targets, ""));
                    break;
                }
                Some((pre, suf)) if spec.len() >= pre.len() + suf.len() && spec.starts_with(pre) && spec.ends_with(suf) => {
                    if best.is_none_or(|(len, _, _)| pre.len() > len) {
                        best = Some((pre.len(), targets, &spec[pre.len()..spec.len() - suf.len()]));
                    }
                }
                _ => {}
            }
        }
        let mut out: Vec<PathBuf> = best
            .map(|(_, targets, star)| targets.iter().map(|t| self.paths_base.join(t.replacen('*', star, 1))).collect())
            .unwrap_or_default();
        out.extend(self.base_url.as_ref().map(|b| b.join(spec)));
        out
    }

    /// What's missing, for a note: `"x" (in tsconfig.base.json), "y"`.
    fn missing_text(&self, root: &Path) -> String {
        self.missing
            .iter()
            .map(|(file, spec)| if *file == self.tsconfig { format!("\"{spec}\"") } else { format!("\"{spec}\" (in {})", rel(root, file)) })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// One note per set of missing bases, naming the tsconfigs they break.
pub fn notes<'a>(root: &Path, fallbacks: impl Iterator<Item = &'a Fallback>) -> Vec<String> {
    let mut by_missing: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for f in fallbacks {
        by_missing.entry(f.missing_text(root)).or_default().push(rel(root, &f.tsconfig));
    }
    by_missing
        .into_iter()
        .map(|(missing, mut names)| {
            // Shallowest first, so the project's own tsconfig leads.
            names.sort_by_key(|n| (n.matches('/').count(), n.clone()));
            let who = match names.as_slice() {
                [one] => format!("imports resolve with {one}'s own `paths` and `baseUrl` only"),
                [first, rest @ ..] => format!(
                    "{first} and {} other {} resolve imports with their own `paths` and `baseUrl` only",
                    rest.len(),
                    if rest.len() == 1 { "tsconfig" } else { "tsconfigs" }
                ),
                [] => unreachable!(),
            };
            format!("tsconfig `extends` {missing} can't be found (is it installed?); {who}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_follow_typescript_matching() {
        let f = Fallback {
            tsconfig: PathBuf::from("/p/tsconfig.json"),
            missing: vec![],
            paths: vec![
                ("@app/*".into(), vec!["src/app/*".into(), "gen/*".into()]),
                ("@app/core/*".into(), vec!["core/*".into()]),
                ("exact".into(), vec!["lib/exact.ts".into()]),
                ("*.css".into(), vec!["styles/*.css".into()]),
            ],
            paths_base: PathBuf::from("/p"),
            base_url: Some(PathBuf::from("/p/src")),
        };
        let c = |s: &str| f.candidates(s).iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();
        assert_eq!(c("@app/x"), ["/p/src/app/x", "/p/gen/x", "/p/src/@app/x"]);
        assert_eq!(c("@app/core/y"), ["/p/core/y", "/p/src/@app/core/y"]); // longest prefix wins
        assert_eq!(c("exact"), ["/p/lib/exact.ts", "/p/src/exact"]);
        assert_eq!(c("theme.css"), ["/p/styles/theme.css", "/p/src/theme.css"]);
        assert_eq!(c("lodash"), ["/p/src/lodash"]);
        assert!(c("./a").is_empty());
    }
}
