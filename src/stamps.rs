//! The files that decide a project's configuration and resolution, stamped
//! so a long-lived project (the ESLint add-on) can tell, with a few stats per
//! lint, whether it must re-open or re-resolve. Recorded at every open
//! attempt, successful or not, so a broken config is retried only after one
//! of these files changes again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::{CONFIG_FILE, Options};

/// What a file decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// What installs changed: a change means re-resolving imports.
    Lockfile,
    /// The config, tsconfigs, bundler configs, .env files: a change means
    /// re-opening the project.
    Config,
}

/// The lockfiles package managers write at the root.
const LOCKFILES: &[&str] = &["package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb"];

/// (modified time, size), or `None` for a missing file.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(p: &Path) -> Stamp {
    let m = std::fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamps {
    root: PathBuf,
    /// Whether the root directory existed.
    root_exists: bool,
    files: BTreeMap<PathBuf, (Kind, Stamp)>,
}

impl Stamps {
    /// The polled set for a project at `root`, opened with the `config`
    /// file (the explicit one, or the one found), and, when its config
    /// loaded, its options:
    /// - detangle.toml, package.json and tsconfig*.json at the root, and
    ///   any of those names appearing at the root later;
    /// - the config file, wherever it is;
    /// - the Vite, webpack and Babel configs and the .env files the options
    ///   name, and the explicit tsconfig;
    /// - every file reached from those tsconfigs through `extends`, above
    ///   the root or in node_modules included;
    /// - the lockfiles at the root.
    pub fn collect(root: &Path, config: Option<&Path>, opts: Option<&Options>) -> Stamps {
        let mut files = BTreeMap::new();
        let mut add = |p: PathBuf, kind: Kind| {
            files.entry(p.clone()).or_insert_with(|| (kind, stamp(&p)));
        };
        for (name, kind) in root_names(root) {
            add(root.join(name), kind);
        }
        add(root.join(CONFIG_FILE), Kind::Config);
        add(root.join("package.json"), Kind::Config);
        if let Some(c) = config {
            add(c.to_path_buf(), Kind::Config);
        }
        let mut tsconfigs: Vec<PathBuf> = root_names(root)
            .into_iter()
            .filter(|(n, _)| is_tsconfig(n))
            .map(|(n, _)| root.join(n))
            .collect();
        if let Some(o) = opts {
            let named = [&o.vite_config, &o.webpack_config, &o.babel_config];
            for rel in named.into_iter().flatten() {
                add(root.join(rel), Kind::Config);
            }
            if let Some(t) = &o.tsconfig {
                tsconfigs.push(root.join(t));
            }
            let env_dir = o.config_env.env_dir.as_ref().map_or_else(|| root.to_path_buf(), |d| root.join(d));
            for name in o.config_env.env_file_names() {
                add(env_dir.join(name), Kind::Config);
            }
        }
        for t in tsconfig_chain(&tsconfigs) {
            add(t, Kind::Config);
        }
        Stamps { root: root.to_path_buf(), root_exists: stamp(root).is_some(), files }
    }

    /// The most significant kind of file that changed since `collect`, if
    /// any. Costs one stat per file and one listing of the root directory
    /// (for files created there; the directory's mtime can't be trusted for
    /// that, as Windows timestamps are coarse enough for two quick creates
    /// to leave it unchanged).
    pub fn check(&self) -> Option<Kind> {
        if stamp(&self.root).is_some() != self.root_exists {
            // The directory itself appeared or went away (a branch switch).
            return Some(Kind::Config);
        }
        let mut changed = self.files.iter().filter(|(p, (_, s))| stamp(p) != *s).map(|(_, (k, _))| *k).max();
        // A relevant name that wasn't there before (existing ones and
        // deletions show up in their stamps).
        for (name, kind) in root_names(&self.root) {
            if !self.files.contains_key(&self.root.join(&name)) {
                changed = changed.max(Some(kind));
            }
        }
        changed
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.files.keys().map(PathBuf::as_path)
    }
}

fn is_tsconfig(name: &str) -> bool {
    name.starts_with("tsconfig") && name.ends_with(".json")
}

/// The root directory's files that are polled, by name.
fn root_names(root: &Path) -> Vec<(String, Kind)> {
    let Ok(dir) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<(String, Kind)> = dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| {
            if LOCKFILES.contains(&n.as_str()) {
                Some((n, Kind::Lockfile))
            } else if n == CONFIG_FILE || n == "package.json" || is_tsconfig(&n) {
                Some((n, Kind::Config))
            } else {
                None
            }
        })
        .collect();
    out.sort();
    out
}

/// Every tsconfig reached from `start` through `extends` (string or array),
/// `start` included, following relative paths and packages in node_modules
/// as TypeScript does. Unreadable or unresolvable entries end their branch.
fn tsconfig_chain(start: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<PathBuf> = start.to_vec();
    while let Some(p) = todo.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        let Ok(mut text) = std::fs::read_to_string(&p) else { continue };
        if json_strip_comments::strip(&mut text).is_err() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let dir = p.parent().unwrap_or(Path::new("."));
        let extends: Vec<&str> = match json.get("extends") {
            Some(serde_json::Value::String(s)) => vec![s],
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => vec![],
        };
        todo.extend(extends.into_iter().filter_map(|spec| resolve_extends(dir, spec)));
    }
    seen
}

/// Where a tsconfig `extends` entry points: a path relative to `dir` (with
/// or without `.json`), or a package (`@tsconfig/node20`, or a file inside
/// one) in the nearest node_modules that has it.
fn resolve_extends(dir: &Path, spec: &str) -> Option<PathBuf> {
    let file = |p: PathBuf| -> Option<PathBuf> {
        let json = PathBuf::from(format!("{}.json", p.display()));
        let found = [p.clone(), json, p.join("tsconfig.json")].into_iter().find(|c| c.is_file())?;
        Some(dunce::canonicalize(&found).unwrap_or(found))
    };
    if spec.starts_with('.') || Path::new(spec).is_absolute() {
        // A missing base is still polled: creating it changes the config.
        return Some(file(dir.join(spec)).unwrap_or_else(|| dir.join(spec)));
    }
    dir.ancestors().find_map(|d| file(d.join("node_modules").join(spec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Tmp {
            let dir = std::env::temp_dir().join(format!("detangle-stamps-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dunce::canonicalize(&dir).unwrap())
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            // Different sizes, so a same-second mtime still shows the change.
            std::fs::write(p, body).unwrap();
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn follows_the_tsconfig_extends_chain() {
        let t = Tmp::new("extends");
        let root = t.0.join("app");
        t.write("base.json", r#"{ "compilerOptions": {} }"#);
        t.write("app/tsconfig.json", "{\n  // comments are allowed\n  \"extends\": [\"../base\", \"@tsconfig/node20/tsconfig.json\", \"@acme/cfg\"],\n}");
        t.write("app/node_modules/@tsconfig/node20/tsconfig.json", "{}");
        t.write("app/node_modules/@acme/cfg/tsconfig.json", r#"{ "extends": "./strict.json" }"#);
        t.write("app/node_modules/@acme/cfg/strict.json", "{}");
        let s = Stamps::collect(&root, None, None);
        let paths: Vec<String> = s.paths().map(|p| p.strip_prefix(&t.0).unwrap().to_string_lossy().replace('\\', "/")).collect();
        for want in [
            "base.json",
            "app/tsconfig.json",
            "app/node_modules/@tsconfig/node20/tsconfig.json",
            "app/node_modules/@acme/cfg/tsconfig.json",
            "app/node_modules/@acme/cfg/strict.json",
        ] {
            assert!(paths.contains(&want.to_string()), "{want} missing from {paths:?}");
        }
        assert_eq!(s.check(), None);
        t.write("app/node_modules/@acme/cfg/strict.json", r#"{ "compilerOptions": { "strict": true } }"#);
        assert_eq!(s.check(), Some(Kind::Config));
    }

    #[test]
    fn classifies_what_changed() {
        let t = Tmp::new("kinds");
        t.write("package.json", "{}");
        t.write("elsewhere/rules.toml", "");
        let config = t.0.join("elsewhere/rules.toml");
        let opts = Options { vite_config: Some("vite.config.mjs".into()), ..Default::default() };
        let s = Stamps::collect(&t.0, Some(&config), Some(&opts));
        assert_eq!(s.check(), None);
        // A lockfile appearing at the root.
        t.write("package-lock.json", "{}");
        assert_eq!(s.check(), Some(Kind::Lockfile));
        let s = Stamps::collect(&t.0, Some(&config), Some(&opts));
        t.write("package-lock.json", r#"{"lockfileVersion":3}"#);
        assert_eq!(s.check(), Some(Kind::Lockfile));
        // A config change outweighs a lockfile change.
        t.write("elsewhere/rules.toml", "[options]");
        assert_eq!(s.check(), Some(Kind::Config));
        // Files that don't exist yet are polled too.
        let s = Stamps::collect(&t.0, Some(&config), Some(&opts));
        t.write("vite.config.mjs", "export default {}");
        assert_eq!(s.check(), Some(Kind::Config));
        let s = Stamps::collect(&t.0, Some(&config), Some(&opts));
        t.write("tsconfig.app.json", "{}");
        assert_eq!(s.check(), Some(Kind::Config));
        // Other files at the root don't count.
        let s = Stamps::collect(&t.0, Some(&config), Some(&opts));
        t.write("README.md", "hi");
        assert_eq!(s.check(), None);
    }
}
