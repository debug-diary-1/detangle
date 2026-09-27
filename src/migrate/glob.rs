//! Glob → regex, for converting other tools' path patterns into tangle's
//! regexes (matched against root-relative paths).

/// How a pattern without glob characters is interpreted.
#[derive(Clone, Copy, PartialEq)]
pub enum Plain {
    /// Exactly that path.
    Exact,
    /// That path or anything below it (a directory).
    Prefix,
}

pub fn has_magic(glob: &str) -> bool {
    glob.contains(['*', '?', '[', '{'])
}

/// Converts a minimatch-style glob to an anchored regex. `prefix` is
/// prepended (e.g. the config's directory relative to the project root).
/// With `match_base`, a pattern without `/` matches a file name in any
/// directory (legacy ESLint `overrides.files`).
pub fn to_regex(glob: &str, plain: Plain, match_base: bool, prefix: &str) -> String {
    let g = glob.trim_start_matches("./");
    let prefix = if prefix.is_empty() || prefix == "." { String::new() } else { format!("{}/", prefix.trim_end_matches('/')) };
    let base = if match_base && !g.contains('/') { "(?:.*/)?" } else { "" };
    if !has_magic(g) {
        let path = escape(&format!("{prefix}{}", g.trim_end_matches('/')));
        return match plain {
            Plain::Exact => format!("^{base}{path}$"),
            Plain::Prefix => format!("^{base}{path}(?:/|$)"),
        };
    }
    format!("^{}{base}{}$", escape(&prefix), translate(g))
}

/// A glob as an unanchored regex (no captures).
pub fn body(g: &str) -> String {
    translate(g.trim_start_matches("./"))
}

/// A piece of a glob: literal regex text, or a wildcard (`*`, `**`, `?`,
/// `{a,b}`), which micromatch-style captures number in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Lit(String),
    Wild(String),
}

/// Splits a glob into literal text and wildcards, each as regex.
pub fn parts(g: &str) -> Vec<Part> {
    let chars: Vec<char> = g.trim_start_matches("./").chars().collect();
    let mut out: Vec<Part> = vec![];
    let lit = |out: &mut Vec<Part>, s: &str| match out.last_mut() {
        Some(Part::Lit(l)) => l.push_str(s),
        _ => out.push(Part::Lit(s.to_string())),
    };
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                out.push(Part::Wild(".*".into()));
                i += 2;
                continue;
            }
            '*' => out.push(Part::Wild("[^/]*".into())),
            '?' => out.push(Part::Wild("[^/]".into())),
            '{' | '[' => {
                // Braces and classes: translate the whole construct.
                let close = if chars[i] == '{' { '}' } else { ']' };
                match chars[i..].iter().position(|&c| c == close) {
                    Some(end) => {
                        let piece: String = chars[i..=i + end].iter().collect();
                        let re = translate(&piece);
                        if chars[i] == '{' { out.push(Part::Wild(re)) } else { lit(&mut out, &re) }
                        i += end + 1;
                        continue;
                    }
                    None => lit(&mut out, &escape(&chars[i].to_string())),
                }
            }
            c => lit(&mut out, &escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

fn translate(g: &str) -> String {
    let chars: Vec<char> = g.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                // `**/` = zero or more directories; a trailing `**` = anything.
                if chars.get(i + 2) == Some(&'/') {
                    out.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    out.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '[' => {
                // Character class: copy through, `[!…]` → `[^…]`.
                match chars[i..].iter().position(|&c| c == ']') {
                    Some(end) if end > 1 => {
                        let class: String = chars[i + 1..i + end].iter().collect();
                        let class = class.strip_prefix('!').map_or(class.clone(), |c| format!("^{c}"));
                        out.push('[');
                        out.push_str(&class);
                        out.push(']');
                        i += end + 1;
                        continue;
                    }
                    _ => out.push_str("\\["),
                }
            }
            '{' => {
                // Brace alternation, possibly nested.
                let mut depth = 0;
                let mut end = None;
                for (j, &c) in chars.iter().enumerate().skip(i) {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(j);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                match end {
                    Some(end) => {
                        let inner: String = chars[i + 1..end].iter().collect();
                        let alts: Vec<String> = split_top_level(&inner).iter().map(|a| translate(a)).collect();
                        out.push_str(&format!("(?:{})", alts.join("|")));
                        i = end + 1;
                        continue;
                    }
                    None => out.push_str("\\{"),
                }
            }
            c => out.push_str(&escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(String::new());
                continue;
            }
            _ => {}
        }
        parts.last_mut().unwrap().push(c);
    }
    parts
}

fn escape(s: &str) -> String {
    fancy_regex::escape(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(glob: &str, plain: Plain, base: bool, prefix: &str, path: &str) -> bool {
        fancy_regex::Regex::new(&to_regex(glob, plain, base, prefix)).unwrap().is_match(path).unwrap()
    }

    #[test]
    fn globs() {
        let e = Plain::Exact;
        assert!(m("src/**/*.ts", e, false, "", "src/a/b/c.ts"));
        assert!(m("src/**/*.ts", e, false, "", "src/c.ts"));
        assert!(!m("src/**/*.ts", e, false, "", "lib/c.ts"));
        assert!(m("**/*.{test,spec}.[jt]s", e, false, "", "a/b.spec.js"));
        assert!(!m("*.ts", e, false, "", "src/a.ts"));
        assert!(m("*.test.ts", e, true, "", "src/deep/a.test.ts")); // match_base
        assert!(m("src/[!_]*.ts", e, false, "", "src/a.ts"));
        assert!(!m("src/[!_]*.ts", e, false, "", "src/_a.ts"));
        assert!(m("src/{a,b/{c,d}}/x.ts", e, false, "", "src/b/d/x.ts"));
        assert!(m("./src/**", e, false, "", "src/anything/here.js"));
    }

    #[test]
    fn capture_parts() {
        assert_eq!(
            parts("src/modules/*/components/*.js"),
            [
                Part::Lit("src/modules/".into()),
                Part::Wild("[^/]*".into()),
                Part::Lit("/components/".into()),
                Part::Wild("[^/]*".into()),
                Part::Lit("\\.js".into()),
            ]
        );
        assert_eq!(parts("a/{b,c}"), [Part::Lit("a/".into()), Part::Wild("(?:b|c)".into())]);
    }

    #[test]
    fn plain_paths() {
        assert!(m("./src/server", Plain::Prefix, false, "", "src/server/db.ts"));
        assert!(m("./src/server/", Plain::Prefix, false, "", "src/server"));
        assert!(!m("./src/server", Plain::Prefix, false, "", "src/serverless/x.ts"));
        assert!(m("src/index.ts", Plain::Exact, false, "", "src/index.ts"));
        assert!(m("client", Plain::Prefix, false, "packages/web", "packages/web/client/a.ts"));
        assert!(m("a.b+c(1)", Plain::Exact, false, "", "a.b+c(1)")); // escaped
    }
}
