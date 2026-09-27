//! Converts known-violation files (a JSON array of `{ type, from, to,
//! rule: { name, severity } }` entries) into a tangle baseline.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::rules::BaselineEntry;

/// Cheap shape check: a JSON array whose entries have `from` and `rule.name`.
pub fn looks_like_known_violations(text: &str) -> bool {
    let t = text.trim_start();
    if !t.starts_with('[') {
        return false;
    }
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Array(a)) => {
            !a.is_empty() && a.iter().take(5).all(|e| e.get("from").is_some_and(Value::is_string) && e.pointer("/rule/name").is_some_and(Value::is_string))
        }
        _ => false,
    }
}

/// `node_modules/@scope/pkg/dist/x.js` → `@scope/pkg` (tangle names npm
/// packages by package name).
fn module_id(s: &str) -> String {
    let Some(rest) = s.rsplit_once("node_modules/").map(|(_, r)| r) else { return s.to_string() };
    let mut parts = rest.split('/');
    match (parts.next(), parts.next()) {
        (Some(scope), Some(name)) if scope.starts_with('@') => format!("{scope}/{name}"),
        (Some(name), _) => name.to_string(),
        _ => s.to_string(),
    }
}

pub fn convert(file: &Path) -> Result<Vec<BaselineEntry>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let Value::Array(items) = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))? else {
        bail!("{} isn't a JSON array", file.display());
    };
    let mut out: Vec<BaselineEntry> = items
        .iter()
        .filter_map(|e| {
            let rule = e.pointer("/rule/name")?.as_str()?.to_string();
            let from = e.get("from")?.as_str()?.to_string();
            let to = e.get("to").and_then(Value::as_str).filter(|t| *t != from).map(module_id);
            Some(BaselineEntry { rule, from, to })
        })
        .collect();
    out.sort_by(|a, b| (&a.rule, &a.from, &a.to).cmp(&(&b.rule, &b.from, &b.to)));
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_converts() {
        let text = r#"[
          {"type":"dependency","from":"src/a.ts","to":"node_modules/@scope/pkg/dist/index.js","rule":{"severity":"error","name":"no-dev"}},
          {"type":"module","from":"src/orphan.ts","to":"src/orphan.ts","rule":{"severity":"info","name":"no-orphans"}},
          {"type":"cycle","from":"src/a.ts","to":"src/b.ts","rule":{"severity":"warn","name":"no-circular"},"cycle":[]}
        ]"#;
        assert!(looks_like_known_violations(text));
        assert!(!looks_like_known_violations(r#"{"forbidden": []}"#));
        assert!(!looks_like_known_violations(r#"[{"name": "x"}]"#));
        let f = std::env::temp_dir().join(format!("tangle-kv-{}.json", std::process::id()));
        std::fs::write(&f, text).unwrap();
        let got = convert(&f).unwrap();
        std::fs::remove_file(&f).unwrap();
        let keys: Vec<(String, String, Option<String>)> = got.into_iter().map(|e| (e.rule, e.from, e.to)).collect();
        assert_eq!(
            keys,
            [
                ("no-circular".into(), "src/a.ts".into(), Some("src/b.ts".into())),
                ("no-dev".into(), "src/a.ts".into(), Some("@scope/pkg".into())),
                ("no-orphans".into(), "src/orphan.ts".into(), None),
            ]
        );
    }
}
