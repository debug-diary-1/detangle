//! File discovery, import extraction (oxc parser) and module resolution
//! (oxc_resolver). Everything per-file runs in parallel.

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_resolver::{
    ResolveError, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use oxc_span::SourceType;
use rayon::prelude::*;
use serde::Serialize;

use crate::config::Options;

pub const SOURCE_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

const NODE_BUILTINS: &[&str] = &[
    "assert", "assert/strict", "async_hooks", "buffer", "child_process", "cluster", "console",
    "constants", "crypto", "dgram", "diagnostics_channel", "dns", "dns/promises", "domain",
    "events", "fs", "fs/promises", "http", "http2", "https", "inspector", "module", "net", "os",
    "path", "path/posix", "path/win32", "perf_hooks", "process", "punycode", "querystring",
    "readline", "readline/promises", "repl", "stream", "stream/consumers", "stream/promises",
    "stream/web", "string_decoder", "sys", "timers", "timers/promises", "tls", "trace_events",
    "tty", "url", "util", "util/types", "v8", "vm", "wasi", "worker_threads", "zlib",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ImportFlags {
    pub type_only: bool,
    pub dynamic: bool,
    pub require: bool,
    pub reexport: bool,
}

impl ImportFlags {
    /// Combine two imports of the same target from the same file: the edge is
    /// type-only / dynamic only if *every* import of it is.
    pub fn merge(self, o: Self) -> Self {
        Self {
            type_only: self.type_only && o.type_only,
            dynamic: self.dynamic && o.dynamic,
            require: self.require || o.require,
            reexport: self.reexport || o.reexport,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Target {
    Local(PathBuf),
    Npm(String),
    Builtin(String),
    Unresolved,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub specifier: String,
    pub flags: ImportFlags,
    pub target: Target,
}

#[derive(Debug)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub imports: Vec<Import>,
    pub parse_errors: usize,
}

pub struct Scan {
    pub files: Vec<ScannedFile>,
    pub walk_ms: f64,
    pub parse_ms: f64,
}

fn globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p)?);
    }
    Ok(Some(b.build()?))
}

/// Walks `dir` (respecting .gitignore) and returns source files, filtered by
/// the include/exclude globs, which are matched against root-relative paths.
pub fn discover(root: &Path, dir: &Path, opts: &Options) -> Result<Vec<PathBuf>> {
    let include = globset(&opts.include)?;
    let exclude = globset(&opts.exclude)?;
    let found = Mutex::new(Vec::new());
    WalkBuilder::new(dir)
        .require_git(false)
        .filter_entry(|e| e.file_name() != "node_modules")
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                let Ok(entry) = entry else { return WalkState::Continue };
                let path = entry.path();
                let is_source = entry.file_type().is_some_and(|t| t.is_file())
                    && path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| SOURCE_EXTS.contains(&e));
                if is_source {
                    let rel = path.strip_prefix(root).unwrap_or(path);
                    let included = include.as_ref().is_none_or(|g| g.is_match(rel));
                    let excluded = exclude.as_ref().is_some_and(|g| g.is_match(rel));
                    if included && !excluded {
                        found.lock().unwrap().push(path.to_path_buf());
                    }
                }
                WalkState::Continue
            })
        });
    let mut files = found.into_inner().unwrap();
    files.sort();
    Ok(files)
}

pub fn scan(root: &Path, dir: &Path, opts: &Options) -> Result<Scan> {
    let t = Instant::now();
    let paths = discover(root, dir, opts)?;
    let walk_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let resolver = make_resolver(root, opts);
    let files = paths
        .into_par_iter()
        .map(|path| scan_file(&resolver, path))
        .collect();
    Ok(Scan { files, walk_ms, parse_ms: t.elapsed().as_secs_f64() * 1000.0 })
}

fn make_resolver(root: &Path, opts: &Options) -> Resolver {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let tsconfig = match &opts.tsconfig {
        Some(p) => TsconfigDiscovery::Manual(TsconfigOptions {
            config_file: root.join(p),
            references: TsconfigReferences::Auto,
        }),
        None => TsconfigDiscovery::Auto,
    };
    Resolver::new(ResolveOptions {
        tsconfig: Some(tsconfig),
        extensions: s(&[
            ".ts", ".tsx", ".d.ts", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json", ".node",
        ]),
        // TS ESM projects write `./foo.js` while the file on disk is `foo.ts`.
        extension_alias: vec![
            (".js".into(), s(&[".ts", ".tsx", ".d.ts", ".js", ".jsx"])),
            (".jsx".into(), s(&[".tsx", ".jsx"])),
            (".mjs".into(), s(&[".mts", ".mjs"])),
            (".cjs".into(), s(&[".cts", ".cjs"])),
        ],
        condition_names: s(&["import", "require", "node", "default", "types"]),
        main_fields: s(&["module", "main", "types"]),
        ..ResolveOptions::default()
    })
}

thread_local! {
    static ALLOC: RefCell<Allocator> = RefCell::new(Allocator::default());
}

fn scan_file(resolver: &Resolver, path: PathBuf) -> ScannedFile {
    let Ok(source) = std::fs::read_to_string(&path) else {
        return ScannedFile { path, imports: vec![], parse_errors: 1 };
    };
    let (raw, parse_errors) = ALLOC.with(|a| {
        let mut alloc = a.borrow_mut();
        alloc.reset();
        extract(&alloc, &path, &source)
    });
    let imports = raw
        .into_iter()
        .filter_map(|(specifier, flags)| {
            let target = resolve(resolver, &path, &specifier)?;
            Some(Import { specifier, flags, target })
        })
        .collect();
    ScannedFile { path, imports, parse_errors }
}

fn extract(alloc: &Allocator, path: &Path, source: &str) -> (Vec<(String, ImportFlags)>, usize) {
    let st = SourceType::from_path(path).unwrap_or_default();
    let ret = Parser::new(alloc, source, st).parse();
    let mut c = Collector::default();
    c.visit_program(&ret.program);
    (c.out, ret.diagnostics.len())
}

pub fn is_builtin(spec: &str) -> bool {
    spec.starts_with("node:") || spec.starts_with("bun:") || NODE_BUILTINS.contains(&spec)
}

pub fn is_bare(spec: &str) -> bool {
    !(spec.starts_with('.') || spec.starts_with('/'))
}

/// `@scope/pkg/deep/file` → `@scope/pkg`, `pkg/deep` → `pkg`.
pub fn package_name(spec: &str) -> &str {
    let mut ends = spec.match_indices('/').map(|(i, _)| i);
    let end = if spec.starts_with('@') { ends.nth(1) } else { ends.next() };
    &spec[..end.unwrap_or(spec.len())]
}

fn package_from_path(p: &Path) -> Option<String> {
    let parts: Vec<_> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let i = parts.iter().rposition(|c| *c == "node_modules")?;
    let first = parts.get(i + 1)?;
    if first.starts_with('@') {
        Some(format!("{first}/{}", parts.get(i + 2)?))
    } else {
        Some(first.to_string())
    }
}

fn resolve(resolver: &Resolver, from: &Path, spec: &str) -> Option<Target> {
    if is_builtin(spec) {
        let name = spec.trim_start_matches("node:");
        return Some(Target::Builtin(name.to_string()));
    }
    match resolver.resolve_file(from, spec) {
        Ok(res) => {
            let p = res.path();
            if p.components().any(|c| c.as_os_str() == "node_modules") {
                let pkg = if is_bare(spec) && !spec.starts_with('#') {
                    package_name(spec).to_string()
                } else {
                    package_from_path(p).unwrap_or_else(|| spec.to_string())
                };
                Some(Target::Npm(pkg))
            } else {
                Some(Target::Local(p.to_path_buf()))
            }
        }
        Err(ResolveError::Ignored(_)) => None,
        Err(ResolveError::Builtin { .. }) => Some(Target::Builtin(spec.to_string())),
        Err(_) if is_bare(spec) && !spec.starts_with('#') => {
            // Types-only packages (`import type { X } from "estree"`) live in @types.
            let pkg = package_name(spec);
            let types = match pkg.strip_prefix('@') {
                Some(scoped) => format!("@types/{}", scoped.replacen('/', "__", 1)),
                None => format!("@types/{pkg}"),
            };
            match resolver.resolve_file(from, &format!("{types}/package.json")) {
                Ok(_) => Some(Target::Npm(pkg.to_string())),
                Err(_) => Some(Target::Unresolved),
            }
        }
        Err(_) => Some(Target::Unresolved),
    }
}

#[derive(Default)]
struct Collector {
    out: Vec<(String, ImportFlags)>,
}

impl Collector {
    fn push(&mut self, spec: &str, flags: ImportFlags) {
        self.out.push((spec.to_string(), flags));
    }
}

fn static_string<'a>(e: &'a Expression<'_>) -> Option<&'a str> {
    match e {
        Expression::StringLiteral(s) => Some(s.value.as_str()),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() => {
            t.quasis.first()?.value.cooked.as_ref().map(|c| c.as_str())
        }
        _ => None,
    }
}

impl<'a> Visit<'a> for Collector {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        let all_type_specifiers = it.specifiers.as_ref().is_some_and(|s| {
            !s.is_empty()
                && s.iter().all(|sp| {
                    matches!(sp, ImportDeclarationSpecifier::ImportSpecifier(s) if s.import_kind.is_type())
                })
        });
        let type_only = it.import_kind.is_type() || all_type_specifiers;
        self.push(it.source.value.as_str(), ImportFlags { type_only, ..Default::default() });
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        if let Some(src) = &it.source {
            let type_only = it.export_kind.is_type()
                || (!it.specifiers.is_empty() && it.specifiers.iter().all(|s| s.export_kind.is_type()));
            self.push(
                src.value.as_str(),
                ImportFlags { type_only, reexport: true, ..Default::default() },
            );
        }
        walk::walk_export_named_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.push(
            it.source.value.as_str(),
            ImportFlags { type_only: it.export_kind.is_type(), reexport: true, ..Default::default() },
        );
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Some(s) = static_string(&it.source) {
            self.push(s, ImportFlags { dynamic: true, ..Default::default() });
        }
        walk::walk_import_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if it.callee.is_specific_id("require") && it.arguments.len() == 1
            && let Some(s) = it.arguments[0].as_expression().and_then(static_string) {
                self.push(s, ImportFlags { require: true, ..Default::default() });
            }
        walk::walk_call_expression(self, it);
    }

    fn visit_ts_import_equals_declaration(&mut self, it: &TSImportEqualsDeclaration<'a>) {
        if let TSModuleReference::ExternalModuleReference(r) = &it.module_reference {
            self.push(
                r.expression.value.as_str(),
                ImportFlags { type_only: it.import_kind.is_type(), require: true, ..Default::default() },
            );
        }
    }

    fn visit_ts_import_type(&mut self, it: &TSImportType<'a>) {
        self.push(it.source.value.as_str(), ImportFlags { type_only: true, ..Default::default() });
        walk::walk_ts_import_type(self, it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(src: &str) -> Vec<(String, ImportFlags)> {
        let alloc = Allocator::default();
        extract(&alloc, Path::new("x.ts"), src).0
    }

    #[test]
    fn extracts_all_import_forms() {
        let got = specs(
            r#"
            import a from "a";
            import type { B } from "b";
            import { type C } from "c";
            export * from "d";
            export { e } from "e";
            const f = await import("f");
            const g = require("g");
            import h = require("h");
            type I = import("i").I;
            function inner() { return require(`j`); }
            "#,
        );
        let names: Vec<_> = got.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        assert!(!got[0].1.type_only);
        assert!(got[1].1.type_only && got[2].1.type_only && got[8].1.type_only);
        assert!(got[3].1.reexport && got[4].1.reexport);
        assert!(got[5].1.dynamic);
        assert!(got[6].1.require && got[7].1.require && got[9].1.require);
    }

    #[test]
    fn package_names() {
        assert_eq!(package_name("react"), "react");
        assert_eq!(package_name("react-dom/client"), "react-dom");
        assert_eq!(package_name("@scope/pkg/deep/x"), "@scope/pkg");
        assert_eq!(package_name("@scope/pkg"), "@scope/pkg");
    }
}
