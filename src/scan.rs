//! File discovery, import extraction (oxc parser) and module resolution
//! (oxc_resolver). Everything per-file runs in parallel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::SystemTime;
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
    Resolution, ResolveError, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use oxc_span::SourceType;
use rayon::prelude::*;
use serde::Serialize;

use crate::aliases::{Aliases, Rewrite};
use crate::config::Options;
use crate::sfc;

pub const SOURCE_EXTS: &[&str] =
    &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "vue", "svelte"];

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
    /// Angular component resource (`templateUrl`, `styleUrl(s)`).
    pub resource: bool,
    /// AMD `define([...])` / `require([...])`, or `/// <amd-dependency>`.
    pub amd: bool,
    /// JSDoc `@import` / `{import("…")}` (with `options.jsdoc_imports`).
    pub jsdoc: bool,
    /// A `/// <reference path|types="…" />` directive.
    pub triple_slash: bool,
    /// A call to one of `options.exotic_require` (`module.require`, …): its
    /// position in that list, plus one (0 = not exotic).
    pub exotic: u8,
    /// `process.getBuiltinModule("fs")` (with `options.builtin_module_calls`).
    pub builtin_call: bool,
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
            resource: self.resource && o.resource,
            amd: self.amd || o.amd,
            jsdoc: self.jsdoc && o.jsdoc,
            triple_slash: self.triple_slash || o.triple_slash,
            exotic: self.exotic.max(o.exotic),
            builtin_call: self.builtin_call || o.builtin_call,
        }
    }

    /// A plain `import`/`export` (not require, dynamic, AMD, a directive…).
    pub fn is_import(self) -> bool {
        !(self.require || self.dynamic || self.amd || self.triple_slash || self.exotic != 0 || self.builtin_call)
    }
}

/// Import forms detected only when asked for, as in other tools.
#[derive(Debug, Clone, Default)]
pub struct Detect {
    pub jsdoc: bool,
    pub builtin_calls: bool,
    /// Callees that act like `require`, e.g. `module.require`.
    pub exotic: Vec<String>,
}

impl Detect {
    pub fn new(opts: &Options) -> Self {
        Detect { jsdoc: opts.jsdoc_imports, builtin_calls: opts.builtin_module_calls, exotic: opts.exotic_require.clone() }
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

/// (modified time, size): a cheap proxy for "content changed".
type Stamp = Option<(SystemTime, u64)>;

fn stamp(p: &Path) -> Stamp {
    let m = std::fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

#[derive(Debug)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub imports: Vec<Import>,
    pub parse_errors: usize,
    /// Unresolved imports, kept so a file can be re-resolved without re-parsing.
    raw: Vec<(String, ImportFlags)>,
    stamp: Stamp,
}

/// What a rebuild had to do.
#[derive(Debug, Default, Clone, Copy)]
pub struct Work {
    pub walked: bool,
    pub reparsed: usize,
    pub reresolved: usize,
    pub walk_ms: f64,
    pub parse_ms: f64,
}

/// A long-lived scan of a project that can be updated incrementally.
pub struct Session {
    root: PathBuf,
    dir: PathBuf,
    opts: Options,
    resolver: Resolvers,
    files: Vec<ScannedFile>,
    index: HashMap<PathBuf, usize>,
    pub work: Work,
}

impl Session {
    /// Full scan: walk, parse and resolve everything.
    pub fn new(root: &Path, dir: &Path, opts: &Options) -> Result<Self> {
        let t = Instant::now();
        let paths = discover(root, dir, opts)?;
        let walk_ms = ms(t);
        let t = Instant::now();
        let resolver = make_resolver(root, opts)?;
        let detect = Detect::new(opts);
        let files: Vec<ScannedFile> = paths.into_par_iter().map(|p| parse_file(p, None, &detect).0).collect();
        let mut s = Session {
            root: root.to_path_buf(),
            dir: dir.to_path_buf(),
            opts: opts.clone(),
            resolver,
            files,
            index: HashMap::new(),
            work: Work::default(),
        };
        s.resolve_all();
        s.reindex();
        let n = s.files.len();
        s.work = Work { walked: true, reparsed: n, reresolved: n, walk_ms, parse_ms: ms(t) };
        Ok(s)
    }

    pub fn files(&self) -> &[ScannedFile] {
        &self.files
    }

    fn reindex(&mut self) {
        self.index = self.files.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
    }

    fn resolve_all(&mut self) {
        let r = &self.resolver;
        self.files.par_iter_mut().for_each(|f| f.resolve(r));
    }

    /// Brings the scan up to date after the given paths changed. Config
    /// changes (tsconfig, package.json) need a fresh `Session` instead.
    pub fn update(&mut self, changed: &[PathBuf]) -> Result<()> {
        let mut work = Work::default();
        // Decide from the filesystem, not from event kinds: watchers (notably
        // macOS FSEvents) often report a plain save as a create.
        let structural = changed.iter().any(|p| match self.index.get(p) {
            Some(_) => !p.is_file(),                                      // removed
            None if p.is_file() => has_source_ext(p),                     // added
            None => p.is_dir() || p.extension().is_none(),                // dir created/removed/renamed
        });

        let t = Instant::now();
        let detect = Detect::new(&self.opts);
        let dirty: Vec<usize> = if structural {
            let paths = discover(&self.root, &self.dir, &self.opts)?;
            work.walked = true;
            work.walk_ms = ms(t);
            let same_set = paths.len() == self.files.len() && paths.iter().zip(&self.files).all(|(p, f)| *p == f.path);
            if !same_set {
                // The set of files changed, so any import may now resolve
                // differently (`./foo` → the new `foo.ts`): re-resolve all
                // with fresh resolver caches, but only re-parse what changed.
                let t = Instant::now();
                let mut old: HashMap<PathBuf, ScannedFile> = self.files.drain(..).map(|f| (f.path.clone(), f)).collect();
                let reused: Vec<(PathBuf, Option<ScannedFile>)> =
                    paths.into_iter().map(|p| { let o = old.remove(&p); (p, o) }).collect();
                let parsed: Vec<(ScannedFile, bool)> =
                    reused.into_par_iter().map(|(p, prev)| parse_file(p, prev, &detect)).collect();
                work.reparsed = parsed.iter().filter(|(_, fresh)| *fresh).count();
                self.files = parsed.into_iter().map(|(f, _)| f).collect();
                self.resolver = make_resolver(&self.root, &self.opts)?;
                self.resolve_all();
                self.reindex();
                work.reresolved = self.files.len();
                work.parse_ms = ms(t);
                self.work = work;
                return Ok(());
            }
            // Same files (e.g. an editor's atomic save): re-parse the reported
            // paths plus anything whose stamp moved.
            let mut v: Vec<usize> = (0..self.files.len())
                .into_par_iter()
                .filter(|&i| stamp(&self.files[i].path) != self.files[i].stamp)
                .collect();
            v.extend(changed.iter().filter_map(|p| self.index.get(p).copied()));
            v
        } else {
            changed.iter().filter_map(|p| self.index.get(p).copied()).collect()
        };
        let mut dirty = dirty;
        dirty.sort_unstable();
        dirty.dedup();

        // Same file set: only re-parse and re-resolve the files that changed.
        let t = Instant::now();
        let r = &self.resolver;
        let updated: Vec<(usize, ScannedFile)> = dirty
            .par_iter()
            .map(|&i| {
                let (mut f, _) = parse_file(self.files[i].path.clone(), None, &detect);
                f.resolve(r);
                (i, f)
            })
            .collect();
        work.reparsed = updated.len();
        work.reresolved = updated.len();
        for (i, f) in updated {
            self.files[i] = f;
        }
        work.parse_ms = ms(t);
        self.work = work;
        Ok(())
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
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
    let filter = crate::graph::PathFilter::new(opts);
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
                    let kept = !filter.active() || filter.keep(&[&rel.to_string_lossy().replace('\\', "/")]);
                    if included && !excluded && kept {
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

fn with_extra(mut base: Vec<String>, extra: &[String]) -> Vec<String> {
    for e in extra {
        if !base.contains(e) {
            base.push(e.clone());
        }
    }
    base
}

/// The main resolver honours tsconfig; `plain` ignores it. A tsconfig that
/// can't be loaded (e.g. `extends` a package that isn't installed) makes the
/// main resolver fail for *every* import in its scope, so failures are
/// retried without it.
struct Resolvers {
    main: Resolver,
    plain: Resolver,
    aliases: Aliases,
    /// `options.resolve.builtins` (a replacement list) and `builtins_add`.
    builtins: Option<Vec<String>>,
    builtins_add: Vec<String>,
}

impl Resolvers {
    fn is_builtin(&self, spec: &str) -> bool {
        let listed = |l: &[String]| l.iter().any(|b| b == spec || b == spec.trim_start_matches("node:"));
        match &self.builtins {
            Some(list) => listed(list),
            None => is_builtin(spec) || listed(&self.builtins_add),
        }
    }

    fn resolve_file(&self, from: &Path, spec: &str) -> Result<Resolution, ResolveError> {
        self.main.resolve_file(from, spec).or_else(|e| match e {
            ResolveError::Ignored(_) => Err(e),
            _ => self.plain.resolve_file(from, spec).map_err(|_| e),
        })
    }
}

fn make_resolver(root: &Path, opts: &Options) -> Result<Resolvers> {
    let aliases = Aliases::load(root, opts)?;
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let tsconfig = match &opts.tsconfig {
        Some(p) => TsconfigDiscovery::Manual(TsconfigOptions {
            config_file: root.join(p),
            references: TsconfigReferences::Auto,
        }),
        None => TsconfigDiscovery::Auto,
    };
    let r = &opts.resolve;
    let main = Resolver::new(ResolveOptions {
        tsconfig: Some(tsconfig),
        extensions: with_extra(
            r.extensions.clone().unwrap_or_else(|| {
                s(&[".ts", ".tsx", ".d.ts", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json", ".node", ".vue", ".svelte"])
            }),
            &aliases.extensions,
        ),
        // TS ESM projects write `./foo.js` while the file on disk is `foo.ts`.
        extension_alias: vec![
            // Prefer TS source, then the real runtime file, then its typings.
            (".js".into(), s(&[".ts", ".tsx", ".js", ".jsx", ".d.ts"])),
            (".jsx".into(), s(&[".tsx", ".jsx"])),
            (".mjs".into(), s(&[".mts", ".mjs"])),
            (".cjs".into(), s(&[".cts", ".cjs"])),
        ],
        condition_names: r.condition_names.clone().unwrap_or_else(|| s(&["import", "require", "node", "default", "types"])),
        main_fields: r.main_fields.clone().unwrap_or_else(|| s(&["module", "main", "types"])),
        main_files: r.main_files.clone().unwrap_or_else(|| s(&["index"])),
        exports_fields: r.exports_fields.as_ref().map_or_else(|| vec![s(&["exports"])], |f| f.iter().map(|x| x.split('.').map(String::from).collect()).collect()),
        alias_fields: r.alias_fields.iter().map(|x| x.split('.').map(String::from).collect()).collect(),
        symlinks: !r.preserve_symlinks,
        yarn_pnp: r.yarn_pnp.unwrap_or_else(|| root.join(".pnp.cjs").is_file() || root.join(".pnp.js").is_file()),
        // webpack's resolve.modules (e.g. `src` or an absolute directory).
        modules: with_extra(s(&["node_modules"]), &aliases.modules),
        ..ResolveOptions::default()
    });
    let plain = main.clone_with_options(ResolveOptions { tsconfig: None, ..main.options().clone() });
    Ok(Resolvers { main, plain, aliases, builtins: r.builtins.clone(), builtins_add: r.builtins_add.clone() })
}

thread_local! {
    static ALLOC: RefCell<Allocator> = RefCell::new(Allocator::default());
}

/// Parses `path`, reusing `prev`'s imports when the file is unchanged
/// (returns whether it actually parsed). The result still needs `resolve`.
fn parse_file(path: PathBuf, prev: Option<ScannedFile>, detect: &Detect) -> (ScannedFile, bool) {
    let st = stamp(&path);
    if let Some(prev) = prev
        && prev.stamp.is_some()
        && prev.stamp == st
    {
        return (ScannedFile { imports: vec![], ..prev }, false);
    }
    let Ok(source) = std::fs::read_to_string(&path) else {
        return (ScannedFile { path, imports: vec![], parse_errors: 1, raw: vec![], stamp: st }, true);
    };
    let (raw, parse_errors) = ALLOC.with(|a| {
        let mut alloc = a.borrow_mut();
        alloc.reset();
        extract(&alloc, &path, &source, detect)
    });
    (ScannedFile { path, imports: vec![], parse_errors, raw, stamp: st }, true)
}

impl ScannedFile {
    #[cfg(test)]
    pub fn for_test(path: PathBuf, imports: Vec<Import>) -> Self {
        ScannedFile { path, imports, parse_errors: 0, raw: vec![], stamp: None }
    }

    fn resolve(&mut self, resolver: &Resolvers) {
        self.imports = self
            .raw
            .iter()
            .filter_map(|(specifier, flags)| {
                let target = resolve(resolver, &self.path, specifier)?;
                Some(Import { specifier: specifier.clone(), flags: *flags, target })
            })
            .collect();
    }
}

fn extract(alloc: &Allocator, path: &Path, source: &str, detect: &Detect) -> (Vec<(String, ImportFlags)>, usize) {
    let mut c = Collector { out: vec![], decorator_depth: 0, detect };
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !matches!(ext, "vue" | "svelte") {
        let st = SourceType::from_path(path).unwrap_or_default();
        // React projects routinely put JSX in plain .js files (CRA, Vite and
        // Babel all accept it); TS files keep their own rules, since
        // `<T>value` casts conflict with JSX there.
        let st = if st.is_javascript() { st.with_jsx(true) } else { st };
        let ret = Parser::new(alloc, source, st).parse();
        c.visit_program(&ret.program);
        c.comments(&ret.program, source);
        return (c.out, ret.diagnostics.len());
    }
    let mut errors = 0;
    for block in sfc::script_blocks(source) {
        if let Some(src) = block.src {
            c.push(src, ImportFlags::default());
            continue;
        }
        let st = match block.lang {
            "ts" => SourceType::ts(),
            "tsx" => SourceType::tsx(),
            _ => SourceType::jsx(),
        };
        let ret = Parser::new(alloc, block.content, st).parse();
        c.visit_program(&ret.program);
        c.comments(&ret.program, block.content);
        errors += ret.diagnostics.len();
    }
    (c.out, errors)
}

/// A resolved path is an npm package if it lives in node_modules.
fn classify(p: &Path, spec: &str) -> Target {
    if !p.components().any(|c| c.as_os_str() == "node_modules") {
        return Target::Local(p.to_path_buf());
    }
    // Prefer the name as written (`lodash/fp` → `lodash`); fall back to the
    // path for aliases, `#imports` and absolute specifiers.
    let from_path = package_from_path(p);
    let pkg = if is_bare(spec) && !spec.starts_with('#') && from_path.as_deref().is_none_or(|n| package_name(spec) == n) {
        package_name(spec).to_string()
    } else {
        from_path.unwrap_or_else(|| spec.to_string())
    };
    Target::Npm(pkg)
}

pub fn has_source_ext(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| SOURCE_EXTS.contains(&e))
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

fn resolve(resolver: &Resolvers, from: &Path, spec: &str) -> Option<Target> {
    // Aliases (webpack / babel / tangle.toml) replace the specifier outright.
    match resolver.aliases.rewrite(spec) {
        Some(Rewrite::Ignore) => return None,
        Some(Rewrite::Candidates(cands)) => {
            for c in &cands {
                if let Ok(res) = resolver.resolve_file(from, c) {
                    return Some(classify(res.path(), c));
                }
            }
            return Some(Target::Unresolved);
        }
        None => {}
    }
    // Babel `root`: bare specifiers are also looked up in these directories.
    if is_bare(spec) && !resolver.is_builtin(spec) {
        for r in &resolver.aliases.roots {
            if let Ok(res) = resolver.resolve_file(from, &r.join(spec).to_string_lossy()) {
                return Some(classify(res.path(), spec));
            }
        }
    }
    if resolver.is_builtin(spec) {
        // `node:fs` ≡ `fs`, but `node:sqlite` / `node:test` only exist with the
        // prefix (plain `sqlite` is an npm package), so keep it for those.
        let bare = spec.trim_start_matches("node:");
        let name = if NODE_BUILTINS.contains(&bare) { bare } else { spec };
        return Some(Target::Builtin(name.to_string()));
    }
    match resolver.resolve_file(from, spec) {
        Ok(res) => Some(classify(res.path(), spec)),
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

struct Collector<'d> {
    out: Vec<(String, ImportFlags)>,
    decorator_depth: usize,
    detect: &'d Detect,
}

static TRIPLE_SLASH: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"^///\s*<(reference\s+(path|types)|amd-dependency\s+path)\s*=\s*["']([^"']+)["']"#).expect("valid")
});
static JSDOC_IMPORT_TAG: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"@import\s[^@]*?\bfrom\s*["']([^"']+)["']"#).expect("valid")
});
static JSDOC_BRACKET: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"\bimport\(\s*["']([^"']+)["']\s*\)"#).expect("valid"));

impl Collector<'_> {
    fn push(&mut self, spec: &str, flags: ImportFlags) {
        self.out.push((spec.to_string(), flags));
    }

    /// Imports written in comments: triple-slash directives and (when
    /// enabled) JSDoc type imports.
    fn comments(&mut self, program: &Program, source: &str) {
        for c in &program.comments {
            let text = c.span.source_text(source);
            if let Some(m) = TRIPLE_SLASH.captures(text) {
                let spec = &m[3];
                let flags = ImportFlags { triple_slash: true, amd: m[1].starts_with("amd"), ..Default::default() };
                // `path` is relative to the file even without "./".
                if m.get(2).is_some_and(|k| k.as_str() == "types") || !is_bare(spec) {
                    self.push(spec, flags);
                } else {
                    self.push(&format!("./{spec}"), flags);
                }
            } else if self.detect.jsdoc && text.starts_with("/**") {
                let flags = ImportFlags { type_only: true, jsdoc: true, ..Default::default() };
                for re in [&*JSDOC_IMPORT_TAG, &*JSDOC_BRACKET] {
                    for m in re.captures_iter(text) {
                        self.push(&m[1], flags);
                    }
                }
            }
        }
    }

    /// AMD dependency arrays: `define([...], f)`, `define("id", [...], f)`,
    /// `require([...], f)`.
    fn amd(&mut self, args: &[Argument]) {
        let Some(Expression::ArrayExpression(a)) = args.iter().filter_map(|a| a.as_expression()).find(|e| !matches!(e, Expression::StringLiteral(_))) else {
            return;
        };
        for e in &a.elements {
            // The special dependencies AMD loaders provide themselves.
            if let Some(s) = e.as_expression().and_then(static_string).filter(|s| !matches!(*s, "require" | "exports" | "module")) {
                self.push(s, ImportFlags { amd: true, ..Default::default() });
            }
        }
    }
}

/// `a`, `a.b`, `a.b.c` as written, for matching callee names.
fn callee_name(e: &Expression) -> Option<String> {
    match e {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::StaticMemberExpression(m) => Some(format!("{}.{}", callee_name(&m.object)?, m.property.name)),
        _ => None,
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

impl<'a> Visit<'a> for Collector<'_> {
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
        let single = (it.arguments.len() == 1).then(|| it.arguments[0].as_expression().and_then(static_string)).flatten();
        let callee = callee_name(&it.callee);
        let exotic = callee.as_deref().and_then(|c| self.detect.exotic.iter().position(|x| x == c));
        let plain = |f: fn(&mut ImportFlags)| {
            let mut flags = ImportFlags::default();
            f(&mut flags);
            flags
        };
        match (callee.as_deref(), single) {
            (Some("require"), Some(s)) => self.push(s, plain(|f| f.require = true)),
            (Some("require" | "define"), _) => self.amd(&it.arguments),
            (Some("process.getBuiltinModule"), Some(s)) if self.detect.builtin_calls => self.push(s, plain(|f| f.builtin_call = true)),
            (Some(_), Some(s)) if exotic.is_some() => {
                self.push(s, ImportFlags { exotic: exotic.map_or(0, |i| (i + 1).min(255) as u8), ..Default::default() })
            }
            _ => {}
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

    fn visit_decorator(&mut self, it: &Decorator<'a>) {
        self.decorator_depth += 1;
        walk::walk_decorator(self, it);
        self.decorator_depth -= 1;
    }

    /// Angular `@Component({ templateUrl, styleUrl, styleUrls })`.
    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        if self.decorator_depth > 0 {
            let urls: Vec<&str> = match it.key.static_name().as_deref() {
                Some("templateUrl" | "styleUrl") => static_string(&it.value).into_iter().collect(),
                Some("styleUrls") => match &it.value {
                    Expression::ArrayExpression(a) => {
                        a.elements.iter().filter_map(|e| e.as_expression().and_then(static_string)).collect()
                    }
                    _ => vec![],
                },
                _ => vec![],
            };
            for url in urls {
                // Angular resolves these relative to the component even without "./".
                let spec = if is_bare(url) { format!("./{url}") } else { url.to_string() };
                self.push(&spec, ImportFlags { resource: true, ..Default::default() });
            }
        }
        walk::walk_object_property(self, it);
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
        extract(&alloc, Path::new("x.ts"), src, &Detect::default()).0
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
    fn amd_directives_jsdoc_and_exotic_requires() {
        let alloc = Allocator::default();
        let src = r#"/// <reference path="e.d.ts" />
/// <reference types="node" />
/// <amd-dependency path="./legacy" />
define("id", ["./a", "require", "exports"], function (a) { require(["./b"], () => {}); });
/** @import { C } from "./c.js" */
/** @param {import("./d.js").D} x */
export const f = (x) => [module.require("./g"), process.getBuiltinModule("fs"), other.require("./no")];
"#;
        let plain: Vec<String> = extract(&alloc, Path::new("x.js"), src, &Detect::default()).0.into_iter().map(|(s, _)| s).collect();
        assert_eq!(plain, ["./a", "./b", "./e.d.ts", "node", "./legacy"]);
        let detect = Detect { jsdoc: true, builtin_calls: true, exotic: vec!["module.require".into()] };
        let got = extract(&alloc, Path::new("x.js"), src, &detect).0;
        let names: Vec<&str> = got.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(names, ["./a", "./b", "./g", "fs", "./e.d.ts", "node", "./legacy", "./c.js", "./d.js"]);
        let f = |i: usize| got[i].1;
        assert!(f(0).amd && f(1).amd && f(2).exotic == 1 && f(3).builtin_call);
        assert!(f(4).triple_slash && f(6).triple_slash && f(6).amd);
        assert!(f(7).jsdoc && f(7).type_only && f(8).jsdoc);
        assert!(!f(0).is_import() && ImportFlags::default().is_import());
    }

    #[test]
    fn resolve_options_pick_conditions_fields_main_files_and_builtins() {
        let root = std::env::temp_dir().join(format!("tangle-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let w = |p: &str, t: &str| {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        w("package.json", r#"{ "name": "r" }"#);
        w("node_modules/cond-pkg/package.json", r#"{ "name": "cond-pkg", "exports": { ".": { "browser": "./b.js", "import": "./i.js", "default": "./d.js" } } }"#);
        for f in ["b", "i", "d"] {
            w(&format!("node_modules/cond-pkg/{f}.js"), "");
        }
        w("node_modules/field-pkg/package.json", r#"{ "name": "field-pkg", "main": "main.js", "browser": { "./main.js": "./browser.js" } }"#);
        w("node_modules/field-pkg/main.js", "");
        w("node_modules/field-pkg/browser.js", "");
        w("src/dir/main.js", "");
        w("src/index.js", "");
        let root = root.canonicalize().unwrap();
        let from = root.join("src/index.js");
        let resolved = |opts: &Options, spec: &str| -> String {
            let r = make_resolver(&root, opts).unwrap();
            match resolve(&r, &from, spec) {
                Some(Target::Local(p)) => p.strip_prefix(&root).unwrap().to_string_lossy().into_owned(),
                Some(Target::Builtin(b)) => format!("builtin:{b}"),
                Some(Target::Npm(n)) => format!("npm:{n}"),
                other => format!("{other:?}"),
            }
        };
        let plain = Options::default();
        let mut browser = Options::default();
        browser.resolve.condition_names = Some(vec!["browser".into(), "import".into()]);
        browser.resolve.alias_fields = vec!["browser".into()];
        browser.resolve.main_files = Some(vec!["main".into(), "index".into()]);
        browser.resolve.builtins_add = vec!["electron".into()];
        let file = |opts: &Options, spec: &str| {
            let r = make_resolver(&root, opts).unwrap();
            r.resolve_file(&from, spec).map(|x| x.path().strip_prefix(&root).unwrap().to_string_lossy().into_owned()).unwrap_or_default()
        };
        assert_eq!(file(&plain, "cond-pkg"), "node_modules/cond-pkg/i.js");
        assert_eq!(file(&browser, "cond-pkg"), "node_modules/cond-pkg/b.js");
        assert_eq!(file(&plain, "field-pkg"), "node_modules/field-pkg/main.js");
        assert_eq!(file(&browser, "field-pkg"), "node_modules/field-pkg/browser.js");
        assert_eq!(resolved(&plain, "./dir"), "Some(Unresolved)");
        assert_eq!(resolved(&browser, "./dir"), "src/dir/main.js");
        assert_eq!(resolved(&plain, "electron"), "Some(Unresolved)");
        assert_eq!(resolved(&browser, "electron"), "builtin:electron");
        let mut only = Options::default();
        only.resolve.builtins = Some(vec!["vscode".into()]);
        assert_eq!(resolved(&only, "vscode"), "builtin:vscode");
        assert_eq!(resolved(&only, "fs"), "Some(Unresolved)");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn angular_component_resources() {
        let got = specs(
            r#"
            import { Component } from "@angular/core";
            @Component({
              selector: "app-root",
              templateUrl: "./app.component.html",
              styleUrls: ["./app.component.css", "theme.scss"],
            })
            export class AppComponent { config = { templateUrl: "not-a-decorator" }; }
            "#,
        );
        let res: Vec<_> = got.iter().filter(|(_, f)| f.resource).map(|(s, _)| s.as_str()).collect();
        assert_eq!(res, ["./app.component.html", "./app.component.css", "./theme.scss"]);
    }

    #[test]
    fn vue_and_svelte_scripts() {
        let alloc = Allocator::default();
        let vue = "<template><Child/></template>\n<script setup lang=\"ts\">\nimport Child from './Child.vue'\nimport type { P } from './types'\n</script>\n";
        let (got, errors) = extract(&alloc, Path::new("A.vue"), vue, &Detect::default());
        assert_eq!(errors, 0);
        assert_eq!(got.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), ["./Child.vue", "./types"]);
        assert!(got[1].1.type_only);
        let svelte = "<script>\n  import Button from './Button.svelte';\n  $: doubled = count * 2;\n</script>\n<Button />";
        let (got, errors) = extract(&alloc, Path::new("B.svelte"), svelte, &Detect::default());
        assert_eq!((got.len(), errors), (1, 0));
    }

    /// Incremental updates must always agree with a fresh full scan.
    #[test]
    fn incremental_matches_full_scan() {
        let tmp = std::env::temp_dir().join(format!("tangle-inc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src/feat")).unwrap();
        let root = std::fs::canonicalize(&tmp).unwrap();
        let w = |rel: &str, body: &str| std::fs::write(root.join(rel), body).unwrap();
        let snapshot = |s: &Session| -> Vec<String> {
            let mut v: Vec<String> = s
                .files()
                .iter()
                .flat_map(|f| {
                    let from = f.path.strip_prefix(&root).unwrap().display().to_string();
                    let mut v: Vec<String> =
                        f.imports.iter().map(|i| format!("{from}: {} -> {:?}", i.specifier, i.target)).collect();
                    v.push(format!("{from} (file)"));
                    v
                })
                .map(|l| l.replace(&root.display().to_string(), "<root>"))
                .collect();
            v.sort();
            v
        };
        let opts = Options::default();
        let src = root.join("src");
        w("src/a.ts", "import './b'; import './c'; import './feat/x';");
        w("src/b.ts", "export {}");
        w("src/feat/x.ts", "import '../b'");
        let mut s = Session::new(&root, &src, &opts).unwrap();

        let mut step = |name: &str, changed: &[&str], expect_walk: bool| {
            let paths: Vec<PathBuf> = changed.iter().map(|p| root.join(p)).collect();
            s.update(&paths).unwrap();
            assert_eq!(s.work.walked, expect_walk, "{name}: walked");
            let fresh = Session::new(&root, &src, &opts).unwrap();
            assert_eq!(snapshot(&s), snapshot(&fresh), "{name}: incremental != full");
        };

        w("src/b.ts", "import './a'");
        step("edit", &["src/b.ts"], false);
        w("src/c.ts", "export {}"); // a.ts's './c' now resolves
        step("add file", &["src/c.ts"], true);
        std::fs::remove_file(root.join("src/b.ts")).unwrap(); // './b' now unresolved
        step("delete file", &["src/b.ts"], true);
        std::fs::rename(root.join("src/feat"), root.join("src/feature")).unwrap();
        step("rename dir", &["src/feat", "src/feature"], true);
        w("src/a.ts", "import './c'; import './x';");
        w("src/a.ts", "import './c'; import './y';"); // same size, same instant
        step("same-size edit", &["src/a.ts"], false);
        w("src/tmp.ts", "import './c'");
        std::fs::rename(root.join("src/tmp.ts"), root.join("src/c.ts")).unwrap(); // atomic save
        step("atomic save", &["src/tmp.ts", "src/c.ts"], false); // file set unchanged: no walk
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn jsx_in_plain_js_files() {
        let alloc = Allocator::default();
        let src = "import Button from './Button';\nexport default () => <div><Button /></div>;\n";
        for file in ["App.js", "App.mjs", "App.cjs", "App.jsx"] {
            let (got, errors) = extract(&alloc, Path::new(file), src, &Detect::default());
            assert_eq!((got.len(), errors), (1, 0), "{file}");
        }
        // TS keeps `<T>x` casts working.
        let (_, errors) = extract(&alloc, Path::new("a.ts"), "const x = <number>y;", &Detect::default());
        assert_eq!(errors, 0);
    }

    #[test]
    fn package_names() {
        assert_eq!(package_name("react"), "react");
        assert_eq!(package_name("react-dom/client"), "react-dom");
        assert_eq!(package_name("@scope/pkg/deep/x"), "@scope/pkg");
        assert_eq!(package_name("@scope/pkg"), "@scope/pkg");
    }
}
