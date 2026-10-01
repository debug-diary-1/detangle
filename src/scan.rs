//! File discovery, import extraction (oxc parser) and module resolution
//! (oxc_resolver). Everything per-file runs in parallel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::SystemTime;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

    /// Compact form for the parse cache: one bit per flag, the exotic index above.
    fn to_bits(self) -> u32 {
        let b = [self.type_only, self.dynamic, self.require, self.reexport, self.resource, self.amd, self.jsdoc, self.triple_slash, self.builtin_call];
        b.iter().enumerate().fold(u32::from(self.exotic) << 16, |acc, (i, &on)| acc | (u32::from(on) << i))
    }

    fn from_bits(b: u32) -> Self {
        let on = |i: u32| b >> i & 1 == 1;
        ImportFlags {
            type_only: on(0),
            dynamic: on(1),
            require: on(2),
            reexport: on(3),
            resource: on(4),
            amd: on(5),
            jsdoc: on(6),
            triple_slash: on(7),
            builtin_call: on(8),
            exotic: (b >> 16) as u8,
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

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Local(PathBuf),
    Npm(String),
    Builtin(String),
    Unresolved,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    /// Shared with the scan's raw imports and the graph's edges.
    pub specifier: Arc<str>,
    pub flags: ImportFlags,
    pub target: Target,
}

/// (modified time, size): a cheap proxy for "content changed".
type Stamp = Option<(SystemTime, u64)>;

fn stamp(p: &Path) -> Stamp {
    stamp_of(&std::fs::metadata(p).ok()?)
}

fn stamp_of(m: &std::fs::Metadata) -> Stamp {
    Some((m.modified().ok()?, m.len()))
}

/// A file's contents and stamp, from a single open.
fn read_source(path: &Path) -> std::io::Result<(String, Stamp)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    let mut source = String::with_capacity(meta.len() as usize + 1);
    file.read_to_string(&mut source)?;
    Ok((source, stamp_of(&meta)))
}

/// On-disk parse cache: each file's imports, reused while the file is
/// unchanged. Files are compared by modification time and size; with the
/// `content` strategy a file whose metadata changed (e.g. a fresh checkout)
/// is still reused when its contents hash the same. The comparison happens
/// in `parse_file`, so each file is stat'ed once.
mod cache {
    use super::*;
    use crate::config::CacheStrategy;
    use std::ffi::OsString;
    use std::hash::{Hash, Hasher};

    /// A compact binary file: it loads several times faster than JSON.
    const FILE: &str = "parse-cache.bin";
    const MAGIC: &[u8] = b"detangle-parse-cache-1\n";

    #[derive(Clone)]
    struct Entry {
        /// Modification time and size.
        meta: String,
        /// Content hash (`content` strategy only; else empty).
        hash: String,
        errors: usize,
        /// (specifier, `ImportFlags::to_bits`)
        imports: Vec<(Arc<str>, u32)>,
    }

    /// A loaded cache: each file's previous scan, plus what's needed to save
    /// the cache again.
    #[derive(Default)]
    pub struct Loaded {
        /// By path; `OsString` hashes much faster than `PathBuf`.
        pub files: HashMap<OsString, Hit>,
        entries: HashMap<String, Entry>,
    }

    pub struct Hit {
        /// The file as last scanned, with the stamp it had then.
        pub file: ScannedFile,
        /// Its content hash (`content` strategy only).
        pub hash: Option<String>,
    }

    fn fingerprint(opts: &Options) -> String {
        let d = Detect::new(opts);
        format!("{}|{:?}|{}|{}|{:?}", env!("CARGO_PKG_VERSION"), opts.cache_strategy, d.jsdoc, d.builtin_calls, d.exotic)
    }

    fn meta(stamp: Stamp) -> Option<String> {
        let (t, len) = stamp?;
        let t = t.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(format!("{}.{:09}:{len}", t.as_secs(), t.subsec_nanos()))
    }

    /// Inverse of `meta`.
    fn parse_meta(m: &str) -> Stamp {
        let (t, len) = m.split_once(':')?;
        let (secs, nanos) = t.split_once('.')?;
        let t = std::time::UNIX_EPOCH + std::time::Duration::new(secs.parse().ok()?, nanos.parse().ok()?);
        Some((t, len.parse().ok()?))
    }

    pub fn hash(bytes: &[u8]) -> String {
        let mut h = rustc_hash::FxHasher::default();
        bytes.hash(&mut h);
        format!("{:016x}:{}", h.finish(), bytes.len())
    }

    /// Length-prefixed strings and little-endian numbers.
    struct Writer(Vec<u8>);

    impl Writer {
        fn num(&mut self, n: usize) {
            self.0.extend_from_slice(&(n as u32).to_le_bytes());
        }
        fn str(&mut self, s: &str) {
            self.num(s.len());
            self.0.extend_from_slice(s.as_bytes());
        }
    }

    struct Reader<'b>(&'b [u8]);

    impl<'b> Reader<'b> {
        fn num(&mut self) -> Option<usize> {
            let (n, rest) = self.0.split_first_chunk::<4>()?;
            self.0 = rest;
            Some(u32::from_le_bytes(*n) as usize)
        }
        fn str(&mut self) -> Option<&'b str> {
            let n = self.num()?;
            let (s, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            std::str::from_utf8(s).ok()
        }
    }

    fn decode(bytes: &[u8], opts: &Options) -> Option<HashMap<String, Entry>> {
        let mut r = Reader(bytes.strip_prefix(MAGIC)?);
        if r.str()? != fingerprint(opts) {
            return None;
        }
        let n = r.num()?;
        let mut entries = HashMap::with_capacity(n);
        for _ in 0..n {
            let rel = r.str()?.to_string();
            let meta = r.str()?.to_string();
            let hash = r.str()?.to_string();
            let errors = r.num()?;
            let count = r.num()?;
            let imports = (0..count).map(|_| Some((Arc::from(r.str()?), r.num()? as u32))).collect::<Option<_>>()?;
            entries.insert(rel, Entry { meta, hash, errors, imports });
        }
        r.0.is_empty().then_some(entries)
    }

    pub fn load(dir: &Path, root: &Path, opts: &Options) -> Loaded {
        let Some(entries) = std::fs::read(dir.join(FILE)).ok().and_then(|b| decode(&b, opts)) else {
            return Loaded::default();
        };
        let content = opts.cache_strategy == CacheStrategy::Content;
        let files = entries
            .iter()
            .map(|(rel, e)| {
                let path = root.join(rel);
                let raw = e.imports.iter().map(|(s, b)| (s.clone(), ImportFlags::from_bits(*b))).collect();
                let key = path.as_os_str().to_os_string();
                let file = ScannedFile { path, imports: vec![], parse_errors: e.errors, raw, stamp: parse_meta(&e.meta), overlaid: false };
                let hash = (content && !e.hash.is_empty()).then(|| e.hash.clone());
                (key, Hit { file, hash })
            })
            .collect();
        Loaded { files, entries }
    }

    /// Saves the cache, reusing loaded entries (and their hashes) where current.
    fn save(dir: &Path, root: &Path, opts: &Options, files: &[ScannedFile], loaded: &HashMap<String, Entry>) -> Result<()> {
        let content = opts.cache_strategy == CacheStrategy::Content;
        let entries: Vec<(String, Entry)> = files
            .par_iter()
            .filter_map(|f| {
                let rel = f.path.strip_prefix(root).ok()?.to_string_lossy().into_owned();
                let m = meta(f.stamp)?;
                if let Some(e) = loaded.get(&rel).filter(|e| e.meta == m) {
                    return Some((rel, e.clone()));
                }
                let imports = f.raw.iter().map(|(s, fl)| (s.clone(), fl.to_bits())).collect();
                let hash = if content { hash(&std::fs::read(&f.path).ok()?) } else { String::new() };
                Some((rel, Entry { meta: m, hash, errors: f.parse_errors, imports }))
            })
            .collect();
        let mut w = Writer(MAGIC.to_vec());
        w.str(&fingerprint(opts));
        w.num(entries.len());
        for (rel, e) in &entries {
            w.str(rel);
            w.str(&e.meta);
            w.str(&e.hash);
            w.num(e.errors);
            w.num(e.imports.len());
            for (spec, bits) in &e.imports {
                w.str(spec);
                w.num(*bits as usize);
            }
        }
        std::fs::create_dir_all(dir)?;
        // Unique per thread, not just per process: several projects in one
        // process (e.g. the Node add-on) may save at once.
        let thread: String = format!("{:?}", std::thread::current().id()).chars().filter(char::is_ascii_digit).collect();
        let tmp = dir.join(format!("{FILE}.{}.{thread}", std::process::id()));
        std::fs::write(&tmp, w.0)?;
        std::fs::rename(tmp, dir.join(FILE))?;
        Ok(())
    }

    impl Loaded {
        pub fn save(&self, dir: &Path, root: &Path, opts: &Options, files: &[ScannedFile]) -> Result<()> {
            save(dir, root, opts, files, &self.entries)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Threads in one process saving the same cache mustn't share a
        /// temp file (one's rename would fail, or move the other's
        /// half-written file into place).
        #[test]
        fn saves_from_two_threads() {
            let dir = std::env::temp_dir().join(format!("detangle-cache-threads-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let opts = Options::default();
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        for _ in 0..500 {
                            save(&dir, &dir, &opts, &[], &HashMap::new()).unwrap();
                        }
                    });
                }
            });
            assert!(decode(&std::fs::read(dir.join(FILE)).unwrap(), &opts).is_some());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub imports: Vec<Import>,
    pub parse_errors: usize,
    /// Unresolved imports, kept so a file can be re-resolved without re-parsing.
    raw: Vec<(Arc<str>, ImportFlags)>,
    stamp: Stamp,
    /// `raw` comes from an editor buffer (`Session::overlay`), not the disk.
    overlaid: bool,
}

/// What a rebuild had to do.
#[derive(Debug, Default, Clone, Copy)]
pub struct Work {
    pub walked: bool,
    pub reparsed: usize,
    pub reresolved: usize,
    /// Some file's resolved imports or parse errors changed, or files came
    /// or went: the graph must be rebuilt.
    pub graph_changed: bool,
    pub scan_ms: f64,
}

/// A long-lived scan of a project that can be updated incrementally.
pub struct Session {
    root: PathBuf,
    dir: PathBuf,
    opts: Options,
    resolver: Resolvers,
    files: Vec<ScannedFile>,
    /// Position of each file, by path; built on the first `update` (one-shot
    /// commands never need it).
    index: HashMap<PathBuf, usize>,
    pub work: Work,
}

impl Session {
    /// Full scan: walk, parse and resolve everything.
    pub fn new(root: &Path, dir: &Path, opts: &Options) -> Result<Self> {
        let t = Instant::now();
        let resolver = make_resolver(root, opts)?;
        let detect = Detect::new(opts);
        let cache_dir = opts.cache.dir(root);
        let cached = cache_dir.as_deref().map(|d| cache::load(d, root, opts)).unwrap_or_default();
        // Walk, parse and resolve in one pass: each file is handled by the
        // walker thread that finds it, so parsing starts with the first file.
        let (fresh, seen) = (AtomicBool::new(false), AtomicUsize::new(0));
        let mut files = walk_sources(root, dir, opts, None, |p| {
            let hit = cached.files.get(p.as_os_str());
            seen.fetch_add(usize::from(hit.is_some()), Ordering::Relaxed);
            let (mut f, how) = parse_file(p, hit.map(|h| &h.file), hit.and_then(|h| h.hash.as_deref()), &detect);
            if how != Scanned::Reused {
                fresh.store(true, Ordering::Relaxed);
            }
            f.resolve(&resolver);
            f
        })?;
        files.sort_by_cached_key(|f| path_key(&f.path));
        // Rewrite the cache only when a file changed or went away.
        let changed = fresh.into_inner() || seen.into_inner() != cached.files.len();
        if let Some(d) = &cache_dir
            && changed
        {
            // A cache that can't be written only costs speed.
            let _ = cached.save(d, root, opts, &files);
        }
        let mut s = Session {
            root: root.to_path_buf(),
            dir: dir.to_path_buf(),
            opts: opts.clone(),
            resolver,
            files,
            index: HashMap::new(),
            work: Work::default(),
        };
        let n = s.files.len();
        s.work = Work { walked: true, reparsed: n, reresolved: n, graph_changed: true, scan_ms: ms(t) };
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
        if self.index.len() != self.files.len() {
            self.reindex();
        }
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
            let same_set = paths.len() == self.files.len() && paths.iter().zip(&self.files).all(|(p, f)| *p == f.path);
            if !same_set {
                // The set of files changed, so any import may now resolve
                // differently (`./foo` → the new `foo.ts`): re-resolve all
                // with fresh resolver caches, but only re-parse what changed.
                let mut old: HashMap<PathBuf, ScannedFile> = self.files.drain(..).map(|f| (f.path.clone(), f)).collect();
                let reused: Vec<(PathBuf, Option<ScannedFile>)> =
                    paths.into_iter().map(|p| { let o = old.remove(&p); (p, o) }).collect();
                let parsed: Vec<(ScannedFile, Scanned)> = reused
                    .into_par_iter()
                    .map(|(p, prev)| match prev {
                        // An editor buffer stays authoritative until its own file changes.
                        Some(prev) if prev.overlaid && !changed.contains(&p) => (prev, Scanned::Reused),
                        Some(prev) if prev.overlaid => parse_file(p, None, None, &detect),
                        prev => parse_file(p, prev.as_ref(), None, &detect),
                    })
                    .collect();
                work.reparsed = parsed.iter().filter(|(_, how)| *how != Scanned::Reused).count();
                self.files = parsed.into_iter().map(|(f, _)| f).collect();
                self.resolver = make_resolver(&self.root, &self.opts)?;
                self.resolve_all();
                self.reindex();
                work.reresolved = self.files.len();
                work.graph_changed = true;
                work.scan_ms = ms(t);
                self.work = work;
                return Ok(());
            }
            // Same files (e.g. an editor's atomic save): re-parse the reported
            // paths plus anything whose stamp moved.
            let mut v: Vec<usize> = (0..self.files.len())
                .into_par_iter()
                .filter(|&i| !self.files[i].overlaid && stamp(&self.files[i].path) != self.files[i].stamp)
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
        let r = &self.resolver;
        let updated: Vec<(usize, ScannedFile)> = dirty
            .par_iter()
            .map(|&i| {
                let (mut f, _) = parse_file(self.files[i].path.clone(), None, None, &detect);
                f.resolve(r);
                (i, f)
            })
            .collect();
        work.reparsed = updated.len();
        work.reresolved = updated.len();
        for (i, f) in updated {
            let old = &self.files[i];
            work.graph_changed |= old.imports != f.imports || old.parse_errors != f.parse_errors;
            self.files[i] = f;
        }
        work.scan_ms = ms(t);
        self.work = work;
        Ok(())
    }

    fn position(&mut self, path: &Path) -> Option<usize> {
        if self.index.len() != self.files.len() {
            self.reindex();
        }
        self.index.get(path).copied()
    }

    /// Whether `path` is one of the scanned files.
    pub fn contains(&mut self, path: &Path) -> bool {
        self.position(path).is_some()
    }

    /// Whether a walk would include `path`: under `dir`, a source file, not
    /// ignored (.gitignore, .ignore, hidden, node_modules) and kept by the
    /// include/exclude globs. It walks only the directories on the way to
    /// `path`, with the walk's own rules, rather than the whole tree.
    pub fn eligible(&self, path: &Path) -> bool {
        walk_sources_along(&self.root, &self.dir, &self.opts, path).is_ok_and(|found| found.iter().any(|p| p == path))
    }

    /// Adds `path`, an eligible file that isn't a member yet: the structural
    /// update its watcher event would bring.
    pub fn admit(&mut self, path: &Path) -> Result<()> {
        self.update(&[path.to_path_buf()])
    }

    /// Makes `source` (an editor buffer) the contents of member `path`.
    /// Returns whether its imports or parse errors changed; if so, only this
    /// file is re-resolved (resolution depends on the set of files and on
    /// config files, which a buffer doesn't change) and `work.graph_changed`
    /// is set. The buffer stays in effect across updates until an update
    /// names `path` itself, which re-reads it from disk.
    pub fn overlay(&mut self, path: &Path, source: &str) -> bool {
        let Some(i) = self.position(path) else { return false };
        let t = Instant::now();
        let detect = Detect::new(&self.opts);
        let (raw, parse_errors) = ALLOC.with(|a| {
            let mut alloc = a.borrow_mut();
            alloc.reset();
            extract(&alloc, path, source, &detect)
        });
        let f = &mut self.files[i];
        if f.raw == raw && f.parse_errors == parse_errors {
            self.work = Work::default();
            return false;
        }
        f.raw = raw;
        f.parse_errors = parse_errors;
        f.overlaid = true;
        f.resolve(&self.resolver);
        self.work = Work { reparsed: 1, reresolved: 1, graph_changed: true, scan_ms: ms(t), ..Default::default() };
        true
    }

    /// Re-resolves every file with a fresh resolver, from the imports
    /// already extracted: no walk, no parse. For when installed packages
    /// changed (a lockfile did), which changes what bare imports resolve to
    /// but not the files or their imports. Overlays are kept.
    pub fn refresh_resolution(&mut self) -> Result<()> {
        let t = Instant::now();
        let before: Vec<Vec<Import>> = self.files.iter_mut().map(|f| std::mem::take(&mut f.imports)).collect();
        self.resolver = make_resolver(&self.root, &self.opts)?;
        self.resolve_all();
        let changed = self.files.iter().zip(&before).any(|(f, b)| f.imports != *b);
        let n = self.files.len();
        self.work = Work { reresolved: n, graph_changed: changed, scan_ms: ms(t), ..Default::default() };
        Ok(())
    }

    /// Whether `dir` contains a scanned file, at any depth.
    pub fn is_member_dir(&self, dir: &Path) -> bool {
        self.files.iter().any(|f| f.path.starts_with(dir))
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
    let mut files = walk_sources(root, dir, opts, None, |p| p)?;
    files.sort_by_cached_key(|p| path_key(p));
    Ok(files)
}

/// A sort key giving `Path::cmp`'s order (component by component) for the
/// normalised paths the walker produces: the bytes, with separators mapped
/// below everything else. Keys compare with memcmp, far faster than
/// comparing paths (13 ms → 1 ms for VS Code's 10k files).
fn path_key(p: &Path) -> Vec<u8> {
    p.as_os_str().as_encoded_bytes().iter().map(|&c| if std::path::is_separator(char::from(c)) { 0 } else { c }).collect()
}

/// The source files a walk would find on the way to `path` (at most `path`
/// itself), visiting only its ancestors.
fn walk_sources_along(root: &Path, dir: &Path, opts: &Options, path: &Path) -> Result<Vec<PathBuf>> {
    walk_sources(root, dir, opts, Some(path), |p| p)
}

/// Like `discover`, but maps each source file with `f` on the walker thread
/// that found it. The results are unordered. With `only`, the walk visits
/// just that path and the directories leading to it.
fn walk_sources<T: Send>(root: &Path, dir: &Path, opts: &Options, only: Option<&Path>, f: impl Fn(PathBuf) -> T + Sync) -> Result<Vec<T>> {
    let include = globset(&opts.include)?;
    let exclude = globset(&opts.exclude)?;
    let filter = crate::graph::PathFilter::new(opts);
    let found = Mutex::new(Vec::new());
    let f = &f;
    let only = only.map(Path::to_path_buf);
    WalkBuilder::new(dir)
        .require_git(false)
        .filter_entry(move |e| e.file_name() != "node_modules" && only.as_deref().is_none_or(|p| p.starts_with(e.path())))
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
                        let item = f(path.to_path_buf());
                        found.lock().unwrap().push(item);
                    }
                }
                WalkState::Continue
            })
        });
    Ok(found.into_inner().unwrap())
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
    /// Tells resolvers apart in the thread-local `DirMemo`s.
    id: u64,
    /// The project root (canonical): workspace packages live under it.
    root: PathBuf,
    /// `options.resolve.preserve_symlinks`: keep linked packages at their
    /// node_modules path.
    preserve_symlinks: bool,
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
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    Ok(Resolvers { id, root: root.to_path_buf(), preserve_symlinks: r.preserve_symlinks, main, plain, aliases, builtins: r.builtins.clone(), builtins_add: r.builtins_add.clone() })
}

thread_local! {
    static ALLOC: RefCell<Allocator> = RefCell::new(Allocator::default());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scanned {
    /// Unchanged since `prev`: its imports were reused.
    Reused,
    /// Its metadata changed but its contents hash the same: imports reused.
    Rehashed,
    Parsed,
}

/// Parses `path`, reusing `prev`'s imports when the file is unchanged: when
/// its stamp is the same, or its contents hash to `prev_hash`. The result
/// still needs `resolve`.
fn parse_file(path: PathBuf, prev: Option<&ScannedFile>, prev_hash: Option<&str>, detect: &Detect) -> (ScannedFile, Scanned) {
    let reuse = |prev: &ScannedFile, stamp: Stamp| ScannedFile { imports: vec![], stamp, ..prev.clone() };
    if let Some(prev) = prev
        && prev.stamp.is_some()
        && prev.stamp == stamp(&path)
    {
        return (reuse(prev, prev.stamp), Scanned::Reused);
    }
    // One open for both the stamp and the contents.
    let (source, st) = match read_source(&path) {
        Ok(r) => r,
        Err(_) => {
            return (ScannedFile { stamp: stamp(&path), path, imports: vec![], parse_errors: 1, raw: vec![], overlaid: false }, Scanned::Parsed);
        }
    };
    if let (Some(prev), Some(h)) = (prev, prev_hash)
        && cache::hash(source.as_bytes()) == h
    {
        return (reuse(prev, st), Scanned::Rehashed);
    }
    let (raw, parse_errors) = ALLOC.with(|a| {
        let mut alloc = a.borrow_mut();
        alloc.reset();
        extract(&alloc, &path, &source, detect)
    });
    (ScannedFile { path, imports: vec![], parse_errors, raw, stamp: st, overlaid: false }, Scanned::Parsed)
}

impl ScannedFile {
    #[cfg(test)]
    pub fn for_test(path: PathBuf, imports: Vec<Import>) -> Self {
        ScannedFile { path, imports, parse_errors: 0, raw: vec![], stamp: None, overlaid: false }
    }

    fn resolve(&mut self, resolver: &Resolvers) {
        DIR_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            m.enter(resolver, &self.path);
            self.imports = self
                .raw
                .iter()
                .filter_map(|(specifier, flags)| {
                    let target = m.resolve(resolver, &self.path, specifier)?;
                    Some(Import { specifier: specifier.clone(), flags: *flags, target })
                })
                .collect();
        });
    }
}

/// Resolutions for the directory a thread is working in. A resolution
/// depends only on the importing file's directory, the tsconfig that governs
/// the file and the specifier, and files of one directory are mostly
/// resolved together on one thread (the walker hands a thread a whole
/// directory), so imports repeated across a directory's files resolve once.
#[derive(Default)]
struct DirMemo {
    resolver: u64,
    dir: PathBuf,
    /// The governing tsconfig, by identity (the resolver caches it).
    tsconfig: Option<std::sync::Arc<oxc_resolver::TsConfig>>,
    targets: rustc_hash::FxHashMap<String, Option<Target>>,
}

thread_local! {
    static DIR_MEMO: RefCell<DirMemo> = RefCell::new(DirMemo::default());
}

impl DirMemo {
    fn enter(&mut self, resolver: &Resolvers, file: &Path) {
        let dir = file.parent().unwrap_or(file);
        // A tsconfig that fails to load governs nothing; resolution then
        // falls back to the plain resolver, which ignores tsconfigs.
        let tsconfig = resolver.main.find_tsconfig(file).ok().flatten();
        let same_tsconfig = match (&self.tsconfig, &tsconfig) {
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        if self.resolver != resolver.id || self.dir != dir || !same_tsconfig {
            self.resolver = resolver.id;
            self.dir = dir.to_path_buf();
            self.tsconfig = tsconfig;
            self.targets.clear();
        }
    }

    fn resolve(&mut self, resolver: &Resolvers, from: &Path, spec: &str) -> Option<Target> {
        if let Some(t) = self.targets.get(spec) {
            return t.clone();
        }
        let t = resolve(resolver, from, spec);
        self.targets.insert(spec.to_string(), t.clone());
        t
    }
}

fn extract(alloc: &Allocator, path: &Path, source: &str, detect: &Detect) -> (Vec<(Arc<str>, ImportFlags)>, usize) {
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
    // Aliases (webpack / babel / detangle.toml) replace the specifier outright.
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
            if let Some(t) = resolve_unbuilt_workspace(resolver, from, spec) {
                return Some(t);
            }
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

/// A workspace package of this project (linked into node_modules, its real
/// directory under the root and outside node_modules) whose `exports` and
/// `main` point at build output that doesn't exist yet, as in a fresh clone
/// of a monorepo: the import resolves into the package's sources, the way
/// TypeScript's `node` module resolution does, by resolving the package's own
/// path (no `exports`; `main`, then an index file) or the subpath in it.
/// Installed packages are never resolved this way.
fn resolve_unbuilt_workspace(resolver: &Resolvers, from: &Path, spec: &str) -> Option<Target> {
    let pkg = package_name(spec);
    let sub = spec[pkg.len()..].trim_start_matches('/');
    let linked = from.ancestors().skip(1).map(|d| d.join("node_modules").join(pkg)).find(|p| p.is_dir())?;
    let real = dunce::canonicalize(&linked).ok()?;
    if !real.starts_with(&resolver.root) || real.components().any(|c| c.as_os_str() == "node_modules") {
        return None;
    }
    // With preserve_symlinks the link is kept, as it is for a built package.
    let base = if resolver.preserve_symlinks { linked } else { real };
    // The package itself is a directory request (a trailing separator), so a
    // sibling file named like it (packages/lib.ts) can't match first.
    let request = if sub.is_empty() {
        format!("{}{}", base.display(), std::path::MAIN_SEPARATOR)
    } else {
        base.join(sub).to_string_lossy().into_owned()
    };
    let res = resolver.plain.resolve_file(from, &request).ok()?;
    Some(classify(res.path(), spec))
}

struct Collector<'d> {
    out: Vec<(Arc<str>, ImportFlags)>,
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
        self.out.push((spec.into(), flags));
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

/// Whether `e` is the callee `name` as written (`a`, `a.b`, `a.b.c`).
fn callee_is(e: &Expression, name: &str) -> bool {
    match e {
        Expression::Identifier(id) => id.name == name,
        Expression::StaticMemberExpression(m) => name
            .strip_suffix(m.property.name.as_str())
            .and_then(|n| n.strip_suffix('.'))
            .is_some_and(|n| callee_is(&m.object, n)),
        _ => false,
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
        let callee = &it.callee;
        let require = matches!(callee, Expression::Identifier(id) if id.name == "require");
        if require && let Some(s) = single {
            self.push(s, ImportFlags { require: true, ..Default::default() });
        } else if require || matches!(callee, Expression::Identifier(id) if id.name == "define") {
            self.amd(&it.arguments);
        } else if let Some(s) = single {
            if self.detect.builtin_calls && callee_is(callee, "process.getBuiltinModule") {
                self.push(s, ImportFlags { builtin_call: true, ..Default::default() });
            } else if let Some(i) = self.detect.exotic.iter().position(|x| callee_is(callee, x)) {
                self.push(s, ImportFlags { exotic: (i + 1).min(255) as u8, ..Default::default() });
            }
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
        extract(&alloc, Path::new("x.ts"), src, &Detect::default()).0.into_iter().map(|(s, f)| (s.to_string(), f)).collect()
    }

    #[test]
    fn path_key_orders_like_path_cmp() {
        let names = ["/a/b", "/a.b", "/a/b/c", "/a-b/c", "/a/b.ts", "/a/bc", "/a/b-c/d", "/a", "/ab", "/a/B", "/a/_b"];
        let mut fast: Vec<&Path> = names.iter().map(Path::new).collect();
        let mut std = fast.clone();
        fast.sort_by_cached_key(|p| path_key(p));
        std.sort();
        assert_eq!(fast, std);
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
        let plain: Vec<String> = extract(&alloc, Path::new("x.js"), src, &Detect::default()).0.into_iter().map(|(s, _)| s.to_string()).collect();
        assert_eq!(plain, ["./a", "./b", "./e.d.ts", "node", "./legacy"]);
        let detect = Detect { jsdoc: true, builtin_calls: true, exotic: vec!["module.require".into()] };
        let got = extract(&alloc, Path::new("x.js"), src, &detect).0;
        let names: Vec<&str> = got.iter().map(|(s, _)| &**s).collect();
        assert_eq!(names, ["./a", "./b", "./g", "fs", "./e.d.ts", "node", "./legacy", "./c.js", "./d.js"]);
        let f = |i: usize| got[i].1;
        assert!(f(0).amd && f(1).amd && f(2).exotic == 1 && f(3).builtin_call);
        assert!(f(4).triple_slash && f(6).triple_slash && f(6).amd);
        assert!(f(7).jsdoc && f(7).type_only && f(8).jsdoc);
        assert!(!f(0).is_import() && ImportFlags::default().is_import());
    }

    #[test]
    fn resolve_options_pick_conditions_fields_main_files_and_builtins() {
        let root = std::env::temp_dir().join(format!("detangle-resolve-{}", std::process::id()));
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
        let root = dunce::canonicalize(&root).unwrap();
        let from = root.join("src/index.js");
        let resolved = |opts: &Options, spec: &str| -> String {
            let r = make_resolver(&root, opts).unwrap();
            match resolve(&r, &from, spec) {
                Some(Target::Local(p)) => p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"),
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
            r.resolve_file(&from, spec).map(|x| x.path().strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/")).unwrap_or_default()
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
    fn import_flags_round_trip_through_bits() {
        let f = ImportFlags { type_only: true, reexport: true, triple_slash: true, builtin_call: true, exotic: 3, ..Default::default() };
        assert_eq!(ImportFlags::from_bits(f.to_bits()), f);
        assert_eq!(ImportFlags::from_bits(ImportFlags::default().to_bits()), ImportFlags::default());
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
        assert_eq!(got.iter().map(|(s, _)| &**s).collect::<Vec<_>>(), ["./Child.vue", "./types"]);
        assert!(got[1].1.type_only);
        let svelte = "<script>\n  import Button from './Button.svelte';\n  $: doubled = count * 2;\n</script>\n<Button />";
        let (got, errors) = extract(&alloc, Path::new("B.svelte"), svelte, &Detect::default());
        assert_eq!((got.len(), errors), (1, 0));
    }

    /// Incremental updates must always agree with a fresh full scan.
    #[test]
    fn incremental_matches_full_scan() {
        let tmp = std::env::temp_dir().join(format!("detangle-inc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src/feat")).unwrap();
        let root = dunce::canonicalize(&tmp).unwrap();
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

        let mut step = |name: &str, changed: &[&str], expect_walk: bool, expect_graph: bool| {
            let paths: Vec<PathBuf> = changed.iter().map(|p| root.join(p)).collect();
            s.update(&paths).unwrap();
            assert_eq!(s.work.walked, expect_walk, "{name}: walked");
            assert_eq!(s.work.graph_changed, expect_graph, "{name}: graph changed");
            let fresh = Session::new(&root, &src, &opts).unwrap();
            assert_eq!(snapshot(&s), snapshot(&fresh), "{name}: incremental != full");
        };

        w("src/b.ts", "import './a'");
        step("edit", &["src/b.ts"], false, true);
        w("src/b.ts", "import './a'; const x = 1;");
        step("edit without import changes", &["src/b.ts"], false, false);
        w("src/b.ts", "import './a'; const = ;");
        step("new parse error", &["src/b.ts"], false, true);
        w("src/c.ts", "export {}"); // a.ts's './c' now resolves
        step("add file", &["src/c.ts"], true, true);
        std::fs::remove_file(root.join("src/b.ts")).unwrap(); // './b' now unresolved
        step("delete file", &["src/b.ts"], true, true);
        std::fs::rename(root.join("src/feat"), root.join("src/feature")).unwrap();
        step("rename dir", &["src/feat", "src/feature"], true, true);
        w("src/a.ts", "import './c'; import './x';");
        w("src/a.ts", "import './c'; import './y';"); // same size, same instant
        step("same-size edit", &["src/a.ts"], false, true);
        w("src/tmp.ts", "import './c'");
        std::fs::rename(root.join("src/tmp.ts"), root.join("src/c.ts")).unwrap(); // atomic save
        step("atomic save", &["src/tmp.ts", "src/c.ts"], false, true); // file set unchanged: no walk
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

    /// A test project in a fresh temp directory.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str, files: &[(&str, &str)]) -> Tmp {
            let dir = std::env::temp_dir().join(format!("detangle-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let t = Tmp(dunce::canonicalize(&dir).unwrap());
            for (rel, body) in files {
                t.write(rel, body);
            }
            t
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.path(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        fn session(&self) -> Session {
            Session::new(&self.0, &self.0, &Options::default()).unwrap()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Every file and resolved import, relative to the root, sorted.
    fn snapshot(s: &Session, root: &Path) -> Vec<String> {
        // Stripped as paths, not replaced in text: Debug output escapes
        // Windows backslashes.
        let target = |t: &Target| match t {
            Target::Local(p) => format!("Local({})", p.strip_prefix(root).unwrap_or(p).display()),
            t => format!("{t:?}"),
        };
        let mut v: Vec<String> = s
            .files()
            .iter()
            .flat_map(|f| {
                let from = f.path.strip_prefix(root).unwrap().display().to_string();
                let mut v: Vec<String> = f.imports.iter().map(|i| format!("{from}: {} -> {}", i.specifier, target(&i.target))).collect();
                v.push(format!("{from} ({} parse errors)", f.parse_errors));
                v
            })
            .collect();
        v.sort();
        v
    }

    /// `s` must equal a fresh scan of `t`'s files with `buffers` written
    /// over them: a copy of the project on disk, so the check doesn't rely
    /// on overlays at all.
    fn assert_matches_disk_plus(s: &Session, t: &Tmp, buffers: &[(&str, &str)]) {
        let copy = Tmp::new(&format!("{}-copy", t.0.file_name().unwrap().to_string_lossy()), &[]);
        for entry in WalkBuilder::new(&t.0).hidden(false).build().flatten() {
            let rel = entry.path().strip_prefix(&t.0).unwrap();
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                copy.write(&rel.to_string_lossy(), &std::fs::read_to_string(entry.path()).unwrap());
            }
        }
        for (rel, body) in buffers {
            copy.write(rel, body);
        }
        assert_eq!(snapshot(s, &t.0), snapshot(&copy.session(), &copy.0));
    }

    const A: &str = "import './b';";

    #[test]
    fn overlay_equal_to_disk_changes_nothing() {
        let t = Tmp::new("overlay-same", &[("src/a.ts", A), ("src/b.ts", "")]);
        let mut s = t.session();
        assert!(!s.overlay(&t.path("src/a.ts"), A));
        assert!(!s.work.graph_changed);
        assert_matches_disk_plus(&s, &t, &[]);
    }

    #[test]
    fn overlay_adds_an_import() {
        let t = Tmp::new("overlay-add", &[("src/a.ts", A), ("src/b.ts", ""), ("src/c.ts", "")]);
        let mut s = t.session();
        let buffer = "import './b'; import './c';";
        assert!(s.overlay(&t.path("src/a.ts"), buffer));
        assert!(s.work.graph_changed);
        assert_eq!(s.work.reresolved, 1);
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", buffer)]);
        // A syntax error counts too.
        assert!(s.overlay(&t.path("src/a.ts"), "import './b'; import ("));
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", "import './b'; import (")]);
    }

    #[test]
    fn overlay_then_saved_to_disk() {
        let t = Tmp::new("overlay-save", &[("src/a.ts", A), ("src/b.ts", ""), ("src/c.ts", "")]);
        let mut s = t.session();
        let buffer = "import './c';";
        s.overlay(&t.path("src/a.ts"), buffer);
        t.write("src/a.ts", buffer);
        s.update(&[t.path("src/a.ts")]).unwrap();
        assert_matches_disk_plus(&s, &t, &[]);
        // The file is plain disk content again: a later structural update re-reads it.
        assert!(!s.files()[s.index[&t.path("src/a.ts")]].overlaid);
    }

    #[test]
    fn overlay_then_disk_reverted() {
        let t = Tmp::new("overlay-revert", &[("src/a.ts", A), ("src/b.ts", ""), ("src/c.ts", "")]);
        let mut s = t.session();
        s.overlay(&t.path("src/a.ts"), "import './c';");
        // e.g. `git checkout src/a.ts`: an event for the file, disk content wins.
        t.write("src/a.ts", A);
        s.update(&[t.path("src/a.ts")]).unwrap();
        assert!(s.work.graph_changed);
        assert_matches_disk_plus(&s, &t, &[]);
    }

    #[test]
    fn overlays_survive_other_files_changing() {
        let t = Tmp::new("overlay-survive", &[("src/a.ts", A), ("src/b.ts", ""), ("src/c.ts", "export {}")]);
        let mut s = t.session();
        let a = "import './b'; import './new';";
        s.overlay(&t.path("src/a.ts"), a);
        // Another file is added (structural: every file re-resolved, and
        // a's buffer import of './new' now resolves).
        t.write("src/new.ts", "");
        s.update(&[t.path("src/new.ts")]).unwrap();
        assert!(s.work.walked);
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", a)]);
        // Another file is saved (incremental).
        t.write("src/c.ts", "import './b';");
        s.update(&[t.path("src/c.ts")]).unwrap();
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", a)]);
        // A second buffer.
        let b = "import './c';";
        s.overlay(&t.path("src/b.ts"), b);
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", a), ("src/b.ts", b)]);
        // A directory event over an unchanged file set (same-set structural).
        s.update(&[t.path("src")]).unwrap();
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", a), ("src/b.ts", b)]);
    }

    #[test]
    fn admit_adds_a_new_file() {
        let t = Tmp::new("admit", &[("src/a.ts", "import './new';")]);
        let mut s = t.session();
        t.write("src/new.ts", "import './a';");
        let new = t.path("src/new.ts");
        assert!(!s.contains(&new));
        assert!(s.eligible(&new));
        s.admit(&new).unwrap();
        assert!(s.contains(&new) && s.work.graph_changed);
        assert_matches_disk_plus(&s, &t, &[]);
    }

    #[test]
    fn eligible_applies_the_walks_rules() {
        let t = Tmp::new(
            "eligible",
            &[
                (".gitignore", "dist/\n*.gen.ts\n"),
                ("src/a.ts", ""),
                ("src/x.gen.ts", ""),
                ("dist/out.ts", ""),
                (".hidden/h.ts", ""),
                ("node_modules/p/index.ts", ""),
                ("src/readme.md", ""),
                ("vendor/v.ts", ""),
            ],
        );
        let opts = Options { exclude: vec!["vendor/**".into()], ..Default::default() };
        let s = Session::new(&t.0, &t.path("src"), &opts).unwrap();
        let eligible = |rel: &str| s.eligible(&t.path(rel));
        assert!(eligible("src/a.ts"));
        for rel in ["src/x.gen.ts", "dist/out.ts", ".hidden/h.ts", "node_modules/p/index.ts", "src/readme.md", "src/missing.ts"] {
            assert!(!eligible(rel), "{rel}");
        }
        // Outside the scanned directory, or excluded by a glob.
        let whole = Session::new(&t.0, &t.0, &opts).unwrap();
        assert!(!s.eligible(&t.path("vendor/v.ts")) && !whole.eligible(&t.path("vendor/v.ts")));
        assert!(whole.eligible(&t.path("src/a.ts")) && !whole.eligible(&t.path("dist/out.ts")));
    }

    #[test]
    fn refresh_resolution_sees_installed_packages() {
        let t = Tmp::new("refresh", &[("package.json", "{}"), ("src/a.ts", "import 'pkg';")]);
        let mut s = t.session();
        let a = t.path("src/a.ts");
        s.overlay(&a, "import 'pkg'; import 'other';");
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", "import 'pkg'; import 'other';")]);
        // `npm install pkg other`.
        t.write("node_modules/pkg/package.json", r#"{ "name": "pkg", "main": "index.js" }"#);
        t.write("node_modules/pkg/index.js", "");
        t.write("node_modules/other/index.js", "");
        s.refresh_resolution().unwrap();
        assert!(s.work.graph_changed && !s.work.walked && s.work.reparsed == 0);
        let targets: Vec<&Target> = s.files()[0].imports.iter().map(|i| &i.target).collect();
        assert_eq!(targets, [&Target::Npm("pkg".into()), &Target::Npm("other".into())]);
        // The overlay was kept, and the result is what a fresh scan finds.
        assert_matches_disk_plus(&s, &t, &[("src/a.ts", "import 'pkg'; import 'other';")]);
        s.refresh_resolution().unwrap();
        assert!(!s.work.graph_changed);
    }

    /// A workspace package whose `exports` and `main` point at build output
    /// that doesn't exist yet (a fresh clone, as Excalidraw's examples
    /// import `@excalidraw/excalidraw`) resolves into its sources, the way
    /// TypeScript's `node` module resolution does. An installed package with
    /// the same broken `exports` stays unresolved.
    #[cfg(unix)]
    #[test]
    fn unbuilt_workspace_packages_resolve_to_their_sources() {
        let unbuilt = r#"{ "name": "@x/lib", "main": "./dist/index.js", "exports": { ".": "./dist/index.js", "./*": { "types": "./dist/types/*.d.ts" } } }"#;
        let t = Tmp::new(
            "unbuilt",
            &[
                ("package.json", r#"{ "workspaces": ["packages/*", "examples/*"] }"#),
                ("packages/lib/package.json", unbuilt),
                ("packages/lib/index.ts", "export const lib = 1;"),
                ("packages/lib/types.ts", "export type T = 1;"),
                // A sibling file named like the package directory mustn't win.
                ("packages/lib.ts", "export const sibling = 1;"),
                ("examples/app/package.json", r#"{ "name": "app" }"#),
                ("examples/app/src/a.ts", "import '@x/lib'; import type { T } from '@x/lib/types'; import '@x/broken'; import '@x/lib/missing';"),
                ("node_modules/@x/broken/package.json", &unbuilt.replace("@x/lib", "@x/broken")),
                ("node_modules/@x/broken/index.js", ""),
            ],
        );
        std::os::unix::fs::symlink("../../packages/lib", t.path("node_modules/@x/lib")).unwrap();
        let targets = |s: &Session| -> Vec<(String, String)> {
            let a = s.files().iter().find(|f| f.path.ends_with("src/a.ts")).unwrap();
            a.imports
                .iter()
                .map(|i| {
                    let target = match &i.target {
                        Target::Local(p) => format!("local {}", p.strip_prefix(&t.0).unwrap().display()),
                        other => format!("{other:?}"),
                    };
                    (i.specifier.to_string(), target)
                })
                .collect()
        };
        let want = |lib: &str, types: &str| {
            vec![
                ("@x/lib".to_string(), lib.to_string()),
                ("@x/lib/types".to_string(), types.to_string()),
                ("@x/broken".to_string(), "Unresolved".to_string()),
                ("@x/lib/missing".to_string(), "Unresolved".to_string()),
            ]
        };
        assert_eq!(targets(&t.session()), want("local packages/lib/index.ts", "local packages/lib/types.ts"));
        // With preserve_symlinks, the link is kept, as for a built package:
        // the import is of an npm package, not a local file.
        let mut opts = Options::default();
        opts.resolve.preserve_symlinks = true;
        let s = Session::new(&t.0, &t.0, &opts).unwrap();
        assert_eq!(targets(&s), want("Npm(\"@x/lib\")", "Npm(\"@x/lib\")"));
    }

    #[test]
    fn member_dirs() {
        let t = Tmp::new("member-dirs", &[("src/a/b.ts", ""), (".next/package.json", "{}")]);
        let s = t.session();
        assert!(s.is_member_dir(&t.0) && s.is_member_dir(&t.path("src")) && s.is_member_dir(&t.path("src/a")));
        assert!(!s.is_member_dir(&t.path(".next")) && !s.is_member_dir(&t.path("src/a/b")));
    }
}
