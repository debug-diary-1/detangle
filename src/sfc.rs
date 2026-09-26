//! Extracts `<script>` blocks from single-file components (Vue, Svelte).
//! A tiny tag scanner — templates and styles don't contribute JS imports.

#[derive(Debug, PartialEq)]
pub struct ScriptBlock<'s> {
    /// `lang` attribute, e.g. `ts`; `js` when absent.
    pub lang: &'s str,
    pub content: &'s str,
    /// `<script src="./x.ts">` — an import of another file.
    pub src: Option<&'s str>,
}

pub fn script_blocks(src: &str) -> Vec<ScriptBlock<'_>> {
    let b = src.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(off) = src[i..].find('<') {
        let at = i + off;
        let rest = &b[at..];
        if rest.starts_with(b"<!--") {
            i = src[at..].find("-->").map_or(src.len(), |e| at + e + 3);
            continue;
        }
        // Component-level blocks start at column 0; this skips `<script>` inside
        // templates and `<svelte:head>` (browser scripts, not module imports).
        let line_start = at == 0 || b[at - 1] == b'\n';
        let is_script = line_start
            && rest.len() > 7
            && rest[..7].eq_ignore_ascii_case(b"<script")
            && (rest[7].is_ascii_whitespace() || rest[7] == b'>');
        if !is_script {
            i = at + 1;
            continue;
        }
        // Find the end of the opening tag, skipping quoted attribute values.
        let mut j = at + 7;
        let mut quote = None;
        while j < b.len() {
            match (quote, b[j]) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, c @ (b'"' | b'\'')) => quote = Some(c),
                (None, b'>') => break,
                _ => {}
            }
            j += 1;
        }
        if j >= b.len() {
            break;
        }
        let attrs = &src[at + 7..j];
        let body = j + 1;
        let (content, next) = if attrs.trim_end().ends_with('/') {
            ("", body)
        } else {
            match src[body..].find("</script") {
                Some(e) => (&src[body..body + e], body + e + 8),
                None => (&src[body..], src.len()),
            }
        };
        out.push(ScriptBlock {
            lang: attr(attrs, "lang").unwrap_or("js"),
            content,
            src: attr(attrs, "src"),
        });
        i = next;
    }
    out
}

fn attr<'s>(attrs: &'s str, name: &str) -> Option<&'s str> {
    let mut from = 0;
    while let Some(p) = attrs[from..].find(name).map(|p| p + from) {
        from = p + name.len();
        let boundary = p == 0 || attrs.as_bytes()[p - 1].is_ascii_whitespace();
        let Some(v) = attrs[from..].trim_start().strip_prefix('=') else { continue };
        if !boundary {
            continue;
        }
        let v = v.trim_start();
        return match v.chars().next()? {
            q @ ('"' | '\'') => v[1..].find(q).map(|end| &v[1..1 + end]),
            _ => Some(&v[..v.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(v.len())]),
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vue_blocks() {
        let src = r#"
<template><div :a="x > 1">{{ '<script>' }}</div></template>
<svelte:head>
  <script src="https://cdn.example/x.js"></script>
</svelte:head>
<!-- <script>import "commented-out"</script> -->
<script lang="ts">export default {}</script>
<script setup lang='tsx' generic="T extends Foo">import A from "./A.vue"</script>
<script src="./logic.ts"></script>
<style>.a{}</style>"#;
        let blocks = script_blocks(src);
        assert_eq!(
            blocks,
            [
                ScriptBlock { lang: "ts", content: "export default {}", src: None },
                ScriptBlock { lang: "tsx", content: "import A from \"./A.vue\"", src: None },
                ScriptBlock { lang: "js", content: "", src: Some("./logic.ts") },
            ]
        );
    }

    #[test]
    fn svelte_blocks() {
        let blocks = script_blocks("<script module>export const x = 1</script>\n<script lang=ts>let y</script><p>hi</p>");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].lang, "ts");
    }

    #[test]
    fn non_ascii_does_not_panic() {
        assert!(script_blocks("<p>héllo ✓ <scrip").is_empty());
    }
}
