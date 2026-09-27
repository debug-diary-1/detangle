//! Groups: named, tagged sets of modules (`[[groups]]` in tangle.toml, or Nx
//! projects discovered on every run).

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
    pub project_type: String,
}

/// Finds Nx projects: every `project.json`, plus `package.json` files with an
/// `nx` section (package-based projects).
pub fn discover_nx(root: &Path) -> Result<Vec<NxProject>> {
    let mut out: Vec<NxProject> = vec![];
    let walker = ignore::WalkBuilder::new(root).require_git(false).filter_entry(|e| e.file_name() != "node_modules").build();
    for entry in walker.flatten() {
        let name = entry.file_name().to_string_lossy();
        if name != "project.json" && name != "package.json" {
            continue;
        }
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let dir = path.parent().unwrap_or(root);
        let rel = dir.strip_prefix(root).unwrap_or(dir).to_string_lossy().replace('\\', "/");
        let (nx, fallback_name) = if name == "project.json" {
            (v.clone(), None)
        } else {
            match v.get("nx") {
                Some(nx) => (nx.clone(), v.get("name").and_then(Value::as_str).map(String::from)),
                None => continue,
            }
        };
        if out.iter().any(|p| p.root == rel) {
            continue; // project.json and package.json in the same folder: one project
        }
        let tags = nx.get("tags").and_then(Value::as_array).map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
        let project_name = nx
            .get("name")
            .and_then(Value::as_str)
            .map(String::from)
            .or(fallback_name)
            .unwrap_or_else(|| if rel.is_empty() { "root".into() } else { rel.rsplit('/').next().unwrap_or(&rel).to_string() });
        let project_type = nx.get("projectType").and_then(Value::as_str).unwrap_or("library").to_string();
        out.push(NxProject { name: project_name, root: rel, tags, project_type });
    }
    Ok(out)
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
