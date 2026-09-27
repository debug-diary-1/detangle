//! Converts madge setups — `.madgerc`, the `madge` key in package.json, or
//! `madge --circular …` in a package.json script — into a circular rule plus
//! the matching options.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use super::Imported;
use crate::config::{Config, Options, Pat, Rule, Severity, ToSpec};

#[derive(Default)]
struct Opts {
    exclude: Vec<String>,
    ts_config: Option<String>,
    webpack_config: Option<String>,
    skip_type_imports: bool,
}

/// `.madgerc` (JSON) or package.json's `madge` key.
pub fn from_rc(file: &Path) -> Result<Option<Imported>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("{} isn't JSON", file.display()))?;
    let v = if file.file_name().is_some_and(|n| n == "package.json") {
        match v.get("madge") {
            Some(m) => m.clone(),
            None => return Ok(None),
        }
    } else {
        v
    };
    let strs = |k: &str| -> Vec<String> {
        v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let skip = |lang: &str| v.pointer(&format!("/detectiveOptions/{lang}/skipTypeImports")) == Some(&Value::Bool(true));
    let mut warnings = vec![];
    if v.get("baseDir").is_some() {
        warnings.push("madge baseDir is ignored — detangle analyses the directory you run it on".into());
    }
    Ok(Some(build(
        Opts {
            exclude: strs("excludeRegExp"),
            ts_config: v.get("tsConfig").and_then(Value::as_str).map(String::from),
            webpack_config: v.get("webpackConfig").and_then(Value::as_str).map(String::from),
            skip_type_imports: skip("ts") || skip("tsx") || skip("es6"),
        },
        warnings,
    )))
}

/// A package.json script such as `madge --circular --extensions ts src`.
/// Returns None unless it runs madge's circular check.
pub fn from_script(cmd: &str) -> Option<Imported> {
    let words: Vec<String> = cmd.split_whitespace().map(|w| w.trim_matches(|c| c == '\'' || c == '"').to_string()).collect();
    let at = words.iter().position(|w| w == "madge" || w.ends_with("/madge"))?;
    let args = &words[at + 1..];
    if !args.iter().any(|a| a == "--circular" || a == "-c") {
        return None;
    }
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--exclude" | "-x" => o.exclude.extend(it.next().cloned()),
            "--ts-config" => o.ts_config = it.next().cloned(),
            "--webpack-config" => o.webpack_config = it.next().cloned(),
            _ => {}
        }
    }
    Some(build(o, vec![]))
}

fn build(o: Opts, warnings: Vec<String>) -> Imported {
    let mut config = Config::empty();
    config.options = Options {
        exclude_path: (!o.exclude.is_empty()).then(|| Pat::any(&o.exclude)),
        tsconfig: o.ts_config,
        webpack_config: o.webpack_config,
        // madge counts `import type` unless told to skip it.
        cycles_ignore_type_only: o.skip_type_imports,
        ..Options::default()
    };
    config.forbidden.push(Rule {
        name: "no-circular".into(),
        severity: Severity::Error,
        comment: Some("Circular dependency (converted from madge --circular).".into()),
        scope: Default::default(),
        from: Default::default(),
        to: ToSpec { circular: Some(true), ..Default::default() },
        module: None,
    });
    let summary = match config.options.exclude_path {
        Some(_) => format!("circular check, {} exclude pattern(s)", o.exclude.len()),
        None => "circular check".into(),
    };
    Imported { config, warnings, summary, known_violations: None }
}
