//! Groups: named, tagged sets of modules (`[[groups]]` in tangle.toml, or Nx
//! projects discovered on every run).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use fancy_regex::Regex;
use serde_json::Value;

use crate::config::Config;

pub struct Group {
    /// The group's type (also one of its tags).
    pub name: String,
    re: Regex,
    pub tags: Vec<String>,
    /// A fixed instance name (Nx project name); otherwise the matched path.
    label: Option<String>,
}

impl Group {
    /// The instance `id` belongs to: (label, root path), if any.
    pub fn instance(&self, id: &str) -> Option<(String, String)> {
        let m = self.re.find(id).ok().flatten().filter(|m| m.start() == 0)?;
        let root = m.as_str().trim_end_matches('/').to_string();
        Some((self.label.clone().unwrap_or_else(|| root.clone()), root))
    }
}

/// All groups: discovered Nx projects (deepest first, so nested projects
/// win), then the `[[groups]]` definitions in order.
pub fn resolve(root: &Path, cfg: &Config) -> Result<Vec<Group>> {
    let mut out = vec![];
    if cfg.options.nx_projects {
        let mut projects = discover_nx(root)?;
        projects.sort_by_key(|p| std::cmp::Reverse(p.root.matches('/').count() + usize::from(!p.root.is_empty())));
        for p in projects {
            let re = if p.root.is_empty() { "^".to_string() } else { format!("^{}(?:/|$)", fancy_regex::escape(&p.root)) };
            let mut tags = p.tags;
            tags.push(format!("projectType:{}", p.project_type));
            tags.extend(p.targets.iter().map(|t| format!("target:{t}")));
            out.push(Group { name: "nx-project".into(), re: Regex::new(&re)?, tags, label: Some(p.name) });
        }
    }
    for g in &cfg.groups {
        let re = Regex::new(&g.path.0).with_context(|| format!("group '{}': invalid regex {:?}", g.name, g.path.0))?;
        let mut tags = g.tags.clone();
        tags.insert(0, g.name.clone());
        out.push(Group { name: g.name.clone(), re, tags, label: None });
    }
    Ok(out)
}

pub struct NxProject {
    pub name: String,
    /// Root-relative directory ("" for a root project).
    pub root: String,
    pub tags: Vec<String>,
    /// "application" (apps and e2e projects) or "library".
    pub project_type: String,
    /// Target names (`build`, `test`, …), for `target:<name>` tags.
    pub targets: Vec<String>,
}

/// Finds Nx projects as Nx does without plugins: every `project.json`, and
/// every `package.json` in the package manager's workspaces (package.json
/// `workspaces`, pnpm-workspace.yaml, lerna.json) or next to a project.json.
/// The root package.json counts only with an `nx` section.
pub fn discover_nx(root: &Path) -> Result<Vec<NxProject>> {
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let workspaces = workspace_globs(root);
    let mut project_jsons: BTreeMap<String, Value> = BTreeMap::new();
    let mut package_jsons: BTreeMap<String, Value> = BTreeMap::new();
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .add_custom_ignore_filename(".nxignore")
        .filter_entry(|e| e.file_name() != "node_modules")
        .build();
    for entry in walker.flatten() {
        let name = entry.file_name().to_string_lossy();
        if name != "project.json" && name != "package.json" {
            continue;
        }
        let Some(v) = read(entry.path()) else { continue };
        let dir = entry.path().parent().unwrap_or(root);
        let rel = dir.strip_prefix(root).unwrap_or(dir).to_string_lossy().replace('\\', "/");
        if name == "project.json" { project_jsons.insert(rel, v) } else { package_jsons.insert(rel, v) };
    }
    let layout = read(&root.join("nx.json")).and_then(|n| n.get("workspaceLayout").cloned()).unwrap_or(Value::Null);
    let strs = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array).map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let mut roots: Vec<&String> = project_jsons.keys().chain(package_jsons.keys()).collect();
    roots.sort();
    roots.dedup();
    let mut out = vec![];
    for rel in roots {
        let project = project_jsons.get(rel);
        let manifest = if rel.is_empty() { "package.json".to_string() } else { format!("{rel}/package.json") };
        let pkg = package_jsons.get(rel).filter(|p| {
            project.is_some() || if rel.is_empty() { p.get("nx").is_some() } else { workspaces.is_match(&manifest) }
        });
        if project.is_none() && pkg.is_none() {
            continue;
        }
        let nx = pkg.and_then(|p| p.get("nx"));
        let mut tags = vec![];
        let mut targets = vec![];
        let mut project_type = None;
        if let Some(p) = pkg {
            tags.push(if p.get("private").and_then(Value::as_bool) == Some(true) { "npm:private" } else { "npm:public" }.to_string());
            tags.extend(strs(p.get("keywords")).into_iter().map(|k| format!("npm:{k}")));
            tags.extend(strs(nx.and_then(|n| n.get("tags"))));
            // Scripts become targets (nx.includedScripts narrows them), plus nx.targets.
            match nx.and_then(|n| n.get("includedScripts")) {
                Some(list) => targets.extend(strs(Some(list))),
                None => targets.extend(p.get("scripts").and_then(Value::as_object).into_iter().flatten().map(|(k, _)| k.clone())),
            }
            targets.extend(nx.and_then(|n| n.get("targets")).and_then(Value::as_object).into_iter().flatten().map(|(k, _)| k.clone()));
            project_type = nx.and_then(|n| n.get("projectType")).and_then(Value::as_str).map(String::from);
            if project_type.is_none() {
                let dir = |k: &str| layout.get(k).and_then(Value::as_str);
                project_type = match (dir("appsDir"), dir("libsDir")) {
                    (Some(a), l) if Some(a) != l && rel.starts_with(a) => Some("application".into()),
                    (_, Some(l)) if rel.starts_with(l) => Some("library".into()),
                    _ => None,
                };
            }
        }
        if let Some(p) = project {
            for t in strs(p.get("tags")) {
                if !tags.contains(&t) {
                    tags.push(t);
                }
            }
            // A target whose executor is explicitly empty isn't one.
            for (name, t) in p.get("targets").and_then(Value::as_object).into_iter().flatten() {
                if t.get("executor").and_then(Value::as_str) != Some("") {
                    targets.push(name.clone());
                }
            }
            project_type = p.get("projectType").and_then(Value::as_str).map(String::from).or(project_type);
        }
        targets.sort();
        targets.dedup();
        let name = project
            .and_then(|p| p.get("name"))
            .or_else(|| nx.and_then(|n| n.get("name")))
            .or_else(|| pkg.and_then(|p| p.get("name")))
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| if rel.is_empty() { "root".into() } else { rel.rsplit('/').next().unwrap_or(rel).to_lowercase() });
        let project_type = match project_type.as_deref() {
            Some("library") => "library",
            Some(_) => "application",
            None => infer_project_type(&root.join(rel)),
        };
        out.push(NxProject { name, root: rel.clone(), tags, project_type: project_type.into(), targets });
    }
    Ok(out)
}

/// Nx's guess for a project without `projectType`.
fn infer_project_type(dir: &Path) -> &'static str {
    if dir.join("tsconfig.lib.json").is_file() {
        return "library";
    }
    if dir.join("tsconfig.app.json").is_file() {
        return "application";
    }
    // A package.json without entry points is taken to be an application.
    let pkg = std::fs::read_to_string(dir.join("package.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    match pkg {
        Some(p) if ["exports", "main", "module", "bin"].iter().all(|k| p.get(k).is_none()) => "application",
        _ => "library",
    }
}

/// package.json paths the package manager treats as workspace packages.
struct Workspaces {
    include: globset::GlobSet,
    exclude: globset::GlobSet,
}

impl Workspaces {
    fn is_match(&self, manifest: &str) -> bool {
        self.include.is_match(manifest) && !self.exclude.is_match(manifest)
    }
}

fn workspace_globs(root: &Path) -> Workspaces {
    let mut patterns: Vec<String> = vec![];
    let pkg = std::fs::read_to_string(root.join("package.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    if let Some(ws) = pkg.as_ref().and_then(|p| p.get("workspaces")) {
        let list = ws.get("packages").unwrap_or(ws);
        patterns.extend(list.as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
        patterns.extend(pnpm_packages(&text));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("lerna.json")) {
        let listed: Vec<String> = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("packages").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|p| p.as_str().map(String::from))
            .collect();
        patterns.extend(if listed.is_empty() { vec!["packages/*".to_string()] } else { listed });
    }
    let glob = |p: &str| {
        let p = p.trim_start_matches("./").trim_end_matches('/');
        let p = if p.ends_with("package.json") { p.to_string() } else { format!("{p}/package.json") };
        globset::GlobBuilder::new(&p).literal_separator(true).build().ok()
    };
    let (neg, pos): (Vec<&String>, Vec<&String>) = patterns.iter().partition(|p| p.starts_with('!'));
    let mut include = globset::GlobSetBuilder::new();
    for p in &pos {
        if let Some(g) = glob(p) {
            include.add(g);
        }
    }
    // Only negations: everything else is included.
    if pos.is_empty()
        && !neg.is_empty()
        && let Some(g) = glob("**")
    {
        include.add(g);
    }
    let mut exclude = globset::GlobSetBuilder::new();
    for p in &neg {
        if let Some(g) = glob(&p[1..]) {
            exclude.add(g);
        }
    }
    Workspaces { include: include.build().unwrap_or_default(), exclude: exclude.build().unwrap_or_default() }
}

/// The `packages` list of a pnpm-workspace.yaml (block or flow style).
fn pnpm_packages(yaml: &str) -> Vec<String> {
    let unquote = |s: &str| s.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
    let mut out = vec![];
    let mut inside = false;
    for line in yaml.lines() {
        let code = line.split(" #").next().unwrap_or("").trim_end();
        if let Some(rest) = code.strip_prefix("packages:") {
            let rest = rest.trim();
            if let Some(flow) = rest.strip_prefix('[') {
                out.extend(flow.trim_end_matches(']').split(',').map(unquote).filter(|s| !s.is_empty()));
            }
            inside = rest.is_empty();
            continue;
        }
        if inside {
            match code.trim_start().strip_prefix('-') {
                Some(item) => out.push(unquote(item)),
                None if code.trim().is_empty() => {}
                None => inside = false,
            }
        }
    }
    out
}
/// Nx tag patterns: `*` (any), `/regex/`, globs with `*`, or exact.
pub fn tag_matches(pattern: &str, tag: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.len() > 2 && pattern.starts_with('/') && pattern.ends_with('/') {
        return Regex::new(&pattern[1..pattern.len() - 1]).ok().and_then(|r| r.is_match(tag).ok()).unwrap_or(false);
    }
    if pattern.contains('*') {
        let re = format!("^{}$", pattern.split('*').map(|p| fancy_regex::escape(p).into_owned()).collect::<Vec<_>>().join(".*"));
        return Regex::new(&re).ok().and_then(|r| r.is_match(tag).ok()).unwrap_or(false);
    }
    pattern == tag
}

pub fn has_any(patterns: &[String], tags: &[String]) -> bool {
    patterns.iter().any(|p| tags.iter().any(|t| tag_matches(p, t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnpm_workspace_packages() {
        let block = "packages:\n  - 'packages/*'\n  - \"apps/**\" # comment\n  - '!**/test/**'\ncatalog:\n  x: 1\n";
        assert_eq!(pnpm_packages(block), ["packages/*", "apps/**", "!**/test/**"]);
        assert_eq!(pnpm_packages("packages: [libs/*, 'tools/x']"), ["libs/*", "tools/x"]);
    }

    #[test]
    fn tag_patterns() {
        assert!(tag_matches("*", "anything"));
        assert!(tag_matches("scope:shop", "scope:shop"));
        assert!(!tag_matches("scope:shop", "scope:shopping"));
        assert!(tag_matches("scope:*", "scope:admin"));
        assert!(!tag_matches("scope:*", "type:ui"));
        assert!(tag_matches("/^type:(ui|util)$/", "type:util"));
        assert!(!tag_matches("/^type:(ui|util)$/", "type:feature"));
    }
}
