//! `.env` file loading for evaluating build configs (Vite / webpack / Babel).
//!
//! Syntax follows `dotenv` + `dotenv-expand`: `KEY=value` lines, optional
//! `export`, `#` comments, single / double / backtick quotes (quoted values
//! may span lines; `\n`-style escapes work in double quotes), and `${VAR}` /
//! `$VAR` expansion (`\$` for a literal `$`) in unquoted and double-quoted
//! values. Single- and backtick-quoted values are literal.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
    Backtick,
}

/// One `KEY=value` entry before expansion.
#[derive(Debug, Clone)]
struct Entry {
    key: String,
    value: String,
    quote: Quote,
}

fn parse(text: &str) -> Result<Vec<Entry>> {
    let mut out = vec![];
    let mut rest = text;
    let mut line_no = 0;
    while !rest.is_empty() {
        line_no += 1;
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        rest = tail;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let trimmed = trimmed.strip_prefix("export ").map_or(trimmed, str::trim_start);
        let Some((key, value)) = trimmed.split_once('=').or_else(|| trimmed.split_once(':')) else {
            bail!("line {line_no}: expected KEY=value");
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
            bail!("line {line_no}: invalid variable name {key:?}");
        }
        let value = value.trim_start();
        let quote = match value.chars().next() {
            Some('\'') => Quote::Single,
            Some('"') => Quote::Double,
            Some('`') => Quote::Backtick,
            _ => Quote::None,
        };
        let value = if quote == Quote::None {
            // An unquoted value ends at a ` #` comment.
            let v = match value.find(" #").or_else(|| value.find("\t#")) {
                Some(i) => &value[..i],
                None => value,
            };
            v.trim_end().trim_end_matches('\r').to_string()
        } else {
            let q = value.chars().next().unwrap();
            // The value may continue over following lines until the closing quote.
            let mut body = value[1..].to_string();
            loop {
                if let Some(end) = closing_quote(&body, q) {
                    body.truncate(end);
                    break;
                }
                if rest.is_empty() {
                    bail!("line {line_no}: unterminated {q} quote");
                }
                let (next, tail) = rest.split_once('\n').unwrap_or((rest, ""));
                rest = tail;
                line_no += 1;
                body.push('\n');
                body.push_str(next.trim_end_matches('\r'));
            }
            if quote == Quote::Double { unescape(&body) } else { body }
        };
        out.push(Entry { key: key.to_string(), value, quote });
    }
    Ok(out)
}

/// Index of the closing quote, skipping backslash-escaped ones in "…".
fn closing_quote(s: &str, q: char) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' && q == '"' {
            escaped = true;
        } else if c == q {
            return Some(i);
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            // Keep `\$` for the expansion step, which turns it into `$`.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Expands `${VAR}`, `${VAR:-default}` and `$VAR` using `lookup`; `\$` is a literal `$`.
fn expand(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < value.len() {
        let c = value[i..].chars().next().unwrap();
        if c == '\\' && bytes.get(i + 1) == Some(&b'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if c == '$' {
            if bytes.get(i + 1) == Some(&b'{') {
                if let Some(end) = value[i + 2..].find('}') {
                    let inner = &value[i + 2..i + 2 + end];
                    let (name, default) = match inner.split_once(":-") {
                        Some((n, d)) => (n, Some(d)),
                        None => (inner, None),
                    };
                    let v = lookup(name).filter(|v| !v.is_empty() || default.is_none());
                    out.push_str(&v.unwrap_or_else(|| default.unwrap_or("").to_string()));
                    i += 2 + end + 1;
                    continue;
                }
            } else {
                let len = value[i + 1..].find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_')).unwrap_or(value.len() - i - 1);
                if len > 0 {
                    out.push_str(&lookup(&value[i + 1..i + 1 + len]).unwrap_or_default());
                    i += 1 + len;
                    continue;
                }
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Loads `files` (later files override earlier ones) from `dir`, then
/// expands references. Missing files are skipped. References resolve with
/// the same precedence the values themselves get: `overrides` (tangle.toml
/// `vars`), then the shell environment, then the merged files.
pub fn load(dir: &Path, files: &[String], overrides: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut merged: BTreeMap<String, Entry> = BTreeMap::new();
    for f in files {
        let path = dir.join(f);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for e in parse(&text).with_context(|| format!("parsing {}", path.display()))? {
            merged.insert(e.key.clone(), e);
        }
    }
    let mut resolved: BTreeMap<String, String> = BTreeMap::new();
    let keys: Vec<String> = merged.keys().cloned().collect();
    for key in keys {
        resolve_key(&key, &merged, overrides, &mut resolved, &mut vec![]);
    }
    Ok(resolved)
}

fn resolve_key(
    key: &str,
    merged: &BTreeMap<String, Entry>,
    overrides: &BTreeMap<String, String>,
    resolved: &mut BTreeMap<String, String>,
    stack: &mut Vec<String>,
) -> Option<String> {
    if let Some(v) = resolved.get(key) {
        return Some(v.clone());
    }
    let e = merged.get(key)?;
    if stack.iter().any(|k| k == key) {
        return Some(String::new()); // self-reference / cycle: expand to empty
    }
    stack.push(key.to_string());
    let value = match e.quote {
        Quote::Single | Quote::Backtick => e.value.clone(),
        Quote::None | Quote::Double => {
            // Collect the needed lookups first (the closure can't borrow `resolved` mutably).
            let mut cache: BTreeMap<String, Option<String>> = BTreeMap::new();
            for name in referenced_names(&e.value) {
                let v = overrides
                    .get(&name)
                    .cloned()
                    .or_else(|| std::env::var(&name).ok())
                    .or_else(|| resolve_key(&name, merged, overrides, resolved, stack));
                cache.insert(name, v);
            }
            expand(&e.value, &|n| cache.get(n).cloned().flatten())
        }
    };
    stack.pop();
    resolved.insert(key.to_string(), value.clone());
    Some(value)
}

fn referenced_names(v: &str) -> Vec<String> {
    let mut names = vec![];
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'$') {
            i += 2;
            continue;
        }
        if b[i] == b'$' {
            if b.get(i + 1) == Some(&b'{') {
                if let Some(end) = v[i + 2..].find('}') {
                    let inner = &v[i + 2..i + 2 + end];
                    names.push(inner.split(":-").next().unwrap_or(inner).to_string());
                    i += end + 3;
                    continue;
                }
            } else {
                let len = v[i + 1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(v.len() - i - 1);
                if len > 0 {
                    names.push(v[i + 1..i + 1 + len].to_string());
                    i += 1 + len;
                    continue;
                }
            }
        }
        i += 1;
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(tag: &str, files: &[(&str, &str)]) -> BTreeMap<String, String> {
        let dir = std::env::temp_dir().join(format!("tangle-dotenv-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let names: Vec<String> = files.iter().map(|(n, _)| n.to_string()).chain(["missing.env".into()]).collect();
        let out = load(&dir, &names, &BTreeMap::new()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        out
    }

    #[test]
    fn syntax() {
        let v = vars("syntax", &[(
            ".env",
            "# comment\n\
             export PLAIN = hello world   # trailing comment\n\
             EMPTY=\n\
             SINGLE='no $EXPANSION \\n here'\n\
             DOUBLE=\"line1\\nline2 \\\"quoted\\\"\"\n\
             HASH=\"not # a comment\"\n\
             MULTI=\"first\n\
             second\"\n\
             BACKTICK=`it's \"fine\"`\n\
             WINDOWS=crlf\r\n",
        )]);
        assert_eq!(v["PLAIN"], "hello world");
        assert_eq!(v["EMPTY"], "");
        assert_eq!(v["SINGLE"], "no $EXPANSION \\n here");
        assert_eq!(v["DOUBLE"], "line1\nline2 \"quoted\"");
        assert_eq!(v["HASH"], "not # a comment");
        assert_eq!(v["MULTI"], "first\nsecond");
        assert_eq!(v["BACKTICK"], "it's \"fine\"");
        assert_eq!(v["WINDOWS"], "crlf");
    }

    #[test]
    fn later_files_override_and_expansion_sees_the_merged_result() {
        let v = vars("merge", &[
            (".env", "HOST=localhost\nURL=http://${HOST}:$PORT/api\nPORT=3000\nPRICE=\\$5\nFALLBACK=${UNSET_TANGLE_VAR:-dflt}\nLOOP=$LOOP-x"),
            (".env.production", "HOST=example.com"),
        ]);
        assert_eq!(v["URL"], "http://example.com:3000/api");
        assert_eq!(v["PRICE"], "$5");
        assert_eq!(v["FALLBACK"], "dflt");
        assert_eq!(v["LOOP"], "-x");
    }

    #[test]
    fn errors_name_the_line() {
        let err = parse("OK=1\nnot a pair\n").unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        let err = parse("A=\"open\nB=2\n").unwrap_err().to_string();
        assert!(err.contains("unterminated"), "{err}");
    }
}
