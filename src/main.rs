mod aliases;
mod config;
mod dotenv;
mod graph;
mod groups;
mod migrate;
mod report;
mod rules;
mod scan;
mod sfc;
mod tui;
mod watch;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::graph::{Graph, ModuleKind};
use crate::report::Paint;
use crate::rules::Violation;

// Parsing and graph building allocate heavily from many threads, where
// mimalloc is much faster than the system allocator (notably on macOS).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "tangle",
    version,
    about = "Blazing-fast dependency analysis and architecture rules for JS/TS projects",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    target: Target,
    /// Keep parse results between runs (default dir: node_modules/.cache/tangle)
    #[arg(long, global = true, num_args = 0..=1, value_name = "DIR")]
    cache: Option<Option<PathBuf>>,
    /// How the cache detects changed files
    #[arg(long, global = true, value_enum)]
    cache_strategy: Option<config::CacheStrategy>,
}

/// `--cache` / `--cache-strategy`, applied to every project opened.
static CACHE_ARGS: std::sync::OnceLock<(Option<Option<PathBuf>>, Option<config::CacheStrategy>)> = std::sync::OnceLock::new();

#[derive(Args, Clone)]
struct Target {
    /// Directory to analyse (the project root is found by walking up to tangle.toml / package.json)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Config file (default: <root>/tangle.toml, else built-in rules)
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Mode for evaluating Vite / webpack configs (overrides config_env.mode)
    #[arg(long)]
    mode: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Explore the dependency graph interactively (default); live-reloads on changes
    Tui {
        #[command(flatten)]
        target: Target,
        /// Don't rebuild when files change
        #[arg(long)]
        no_watch: bool,
    },
    /// Re-run the rules whenever files change
    Watch {
        #[command(flatten)]
        target: Target,
    },
    /// Check the rules; exits non-zero when errors are found
    Check {
        #[command(flatten)]
        target: Target,
        #[arg(short, long, value_enum, default_value_t = CheckFormat::Text)]
        format: CheckFormat,
        /// Also fail on warnings
        #[arg(long)]
        strict: bool,
        /// Ignore violations recorded in this baseline file
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Record all current violations as the baseline and exit 0 (default
        /// file: options.baseline, else .tangle-baseline.json)
        #[arg(long, num_args = 0..=1, value_name = "FILE")]
        write_baseline: Option<Option<PathBuf>>,
        /// How --write-baseline updates an existing baseline
        #[arg(long, value_enum, default_value_t = BaselineMode::Full, requires = "write_baseline")]
        baseline_mode: BaselineMode,
    },
    /// Export the dependency graph (dot, mermaid, json)
    Graph {
        #[command(flatten)]
        target: Target,
        #[arg(short, long, value_enum, default_value_t = GraphFormat::Dot)]
        format: GraphFormat,
        /// Collapse local modules to their first N path segments (e.g. 2 → src/feature/),
        /// or modules matching a regex to what it matches (e.g. '^packages/[^/]+/')
        #[arg(long)]
        collapse: Option<String>,
        /// Only show modules matching this regex, plus their neighbours
        #[arg(long)]
        focus: Option<String>,
        /// How many steps from a --focus module to show, in both directions
        #[arg(long, default_value_t = 1, requires = "focus")]
        focus_depth: usize,
        /// Only show modules matching this regex, plus everything that depends on them
        #[arg(long)]
        reaches: Option<String>,
        /// Mark modules matching this regex
        #[arg(long)]
        highlight: Option<String>,
        /// Start from the modules matching this regex: only show what they (indirectly) import
        #[arg(long)]
        from: Option<String>,
        /// With --from, follow imports at most this many steps
        #[arg(long, requires = "from")]
        max_depth: Option<usize>,
        /// Include npm packages, node builtins and unresolved imports
        #[arg(long)]
        externals: bool,
        /// Omit type-only imports
        #[arg(long)]
        no_types: bool,
        /// Write to a file instead of stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Explain why FROM depends on TO (shortest import chain)
    Why {
        from: String,
        to: String,
        #[arg(short = 'C', long = "dir", default_value = ".")]
        dir: PathBuf,
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Mode for evaluating Vite / webpack configs
        #[arg(long)]
        mode: Option<String>,
    },
    /// List modules affected by changes to the given files (transitive dependents)
    Affected {
        files: Vec<String>,
        /// Use files changed since this git ref (e.g. origin/main)
        #[arg(long)]
        since: Option<String>,
        #[arg(short = 'C', long = "dir", default_value = ".")]
        dir: PathBuf,
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Mode for evaluating Vite / webpack configs
        #[arg(long)]
        mode: Option<String>,
        /// Only print affected modules matching this regex (e.g. '\.test\.ts$')
        #[arg(long)]
        filter: Option<String>,
    },
    /// Write a self-contained HTML report (violations, modules, cycles, folder graph)
    Report {
        #[command(flatten)]
        target: Target,
        /// Output file
        #[arg(short, long, default_value = "tangle-report.html")]
        output: PathBuf,
        /// Open it in the browser afterwards
        #[arg(long)]
        open: bool,
    },
    /// Summary statistics and hotspots
    Stats {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Convert existing dependency rules (JS rules configs, ESLint import
    /// rules, madge) and known-violation files into tangle.toml
    Migrate {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the tangle.toml instead of writing it
        #[arg(long)]
        dry_run: bool,
        /// Overwrite an existing tangle.toml
        #[arg(long)]
        force: bool,
    },
    /// Write a starter tangle.toml
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Convert a JavaScript rules config (.js/.cjs/.mjs/.json) to tangle.toml
        #[arg(long, value_name = "FILE")]
        from: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CheckFormat {
    Text,
    Json,
    /// GitHub Actions annotations
    Github,
    /// Markdown, e.g. for a pull-request comment or job summary
    Markdown,
    /// TeamCity service messages
    Teamcity,
    /// Azure DevOps logging commands
    Azure,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum BaselineMode {
    /// Record every current violation
    Full,
    /// Only drop entries that no longer occur; never add new ones
    ShrinkOnly,
}

#[derive(Clone, Copy, ValueEnum)]
enum GraphFormat {
    Dot,
    Mermaid,
    Json,
    D2,
    /// Adjacency matrix
    Csv,
}

pub struct Analysis {
    pub graph: Graph,
    /// Violations, minus those in the configured baseline.
    pub violations: Vec<Violation>,
    pub config_path: Option<PathBuf>,
    /// How many violations the baseline suppressed.
    pub suppressed: usize,
    /// Baseline entries that no longer occur.
    pub stale: Vec<rules::BaselineEntry>,
}

/// A loaded project whose scan can be kept up to date incrementally.
pub struct Project {
    dir: PathBuf,
    root: PathBuf,
    config_arg: Option<PathBuf>,
    mode_arg: Option<String>,
    cfg: Config,
    config_path: Option<PathBuf>,
    /// Notes from loading the config (e.g. JavaScript config import warnings).
    notes: Vec<String>,
    /// `[[groups]]` and discovered Nx projects.
    groups: Vec<groups::Group>,
    session: scan::Session,
}

impl Project {
    fn open(path: &Path, config: Option<&Path>, mode: Option<&str>) -> Result<Self> {
        let dir = std::fs::canonicalize(path).with_context(|| format!("{} not found", path.display()))?;
        if !dir.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        let root = config::find_root(&dir);
        let t = std::time::Instant::now();
        let loaded = config::load(&root, config)?;
        let mut cfg: Config = loaded.config;
        if let Some(m) = mode {
            cfg.options.config_env.mode = Some(m.to_string());
        }
        if let Some((cache, strategy)) = CACHE_ARGS.get() {
            match cache {
                Some(Some(d)) => cfg.options.cache = config::CacheSetting::Dir(std::path::absolute(d)?.to_string_lossy().into_owned()),
                Some(None) => cfg.options.cache = config::CacheSetting::Enabled(true),
                None => {}
            }
            if let Some(s) = strategy {
                cfg.options.cache_strategy = *s;
            }
        }
        rules::validate(&cfg).with_context(|| match &loaded.path {
            Some(p) => format!("in {}", p.display()),
            None => "in the built-in rules".into(),
        })?;
        timing("config", t);
        let t = std::time::Instant::now();
        let session = scan::Session::new(&root, &dir, &cfg.options)?;
        timing("scan", t);
        let t = std::time::Instant::now();
        let groups = groups::resolve(&root, &cfg)?;
        timing("groups", t);
        Ok(Project {
            groups,
            dir,
            root,
            config_arg: config.map(Path::to_path_buf),
            mode_arg: mode.map(String::from),
            cfg,
            config_path: loaded.path,
            notes: loaded.notes,
            session,
        })
    }

    fn analyze(&self) -> Result<Analysis> {
        self.analyze_with(true)
    }

    /// The configured baseline file, if any (it may not exist yet).
    fn baseline_path(&self) -> Option<PathBuf> {
        self.cfg.options.baseline.as_ref().map(|b| self.root.join(b))
    }

    fn analyze_with(&self, use_baseline: bool) -> Result<Analysis> {
        let t = std::time::Instant::now();
        let mut graph = Graph::build(&self.root, self.session.files(), self.session.work, &self.cfg.options);
        graph.assign_groups(&self.groups, self.cfg.options.group_match == config::GroupMatch::Deepest);
        timing("graph", t);
        let t = std::time::Instant::now();
        let mut violations = rules::evaluate(&graph, &self.cfg)?;
        timing("rules", t);
        let used = match self.baseline_path().filter(|p| use_baseline && p.is_file()) {
            Some(p) => rules::apply_baseline(&graph, &mut violations, &p)?,
            None => Default::default(),
        };
        Ok(Analysis { graph, violations, config_path: self.config_path.clone(), suppressed: used.suppressed, stale: used.stale })
    }

    /// Applies filesystem changes and re-analyses. Returns the new analysis
    /// and a one-line description of the work done. On error (e.g. a
    /// half-edited tangle.toml) the previous state is kept.
    fn rebuild(&mut self, changes: &watch::Changes) -> Result<(Analysis, String)> {
        let t = std::time::Instant::now();
        if changes.config {
            *self = Project::open(&self.dir, self.config_arg.as_deref(), self.mode_arg.as_deref())?;
        } else {
            self.session.update(&changes.paths.iter().cloned().collect::<Vec<_>>())?;
        }
        let a = self.analyze()?;
        let w = self.session.work;
        let work = if changes.config {
            format!("full rebuild of {} files", w.reparsed)
        } else if w.walked && w.reresolved > w.reparsed {
            format!("{} reparsed, all re-resolved", w.reparsed)
        } else {
            report::plural(w.reparsed, "file") + " reparsed"
        };
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let what = if changes.paths.is_empty() { String::new() } else { format!("{} · ", changes.describe(&self.root)) };
        Ok((a, format!("↻ {what}{work} · {ms:.0}ms")))
    }
}

/// Analyses a project for a command that exits afterwards.
fn analyze(path: &Path, config: Option<&Path>, mode: Option<&str>) -> Result<&'static mut Analysis> {
    let project = one_shot(Project::open(path, config, mode)?.announce());
    Ok(one_shot(project.analyze()?))
}

/// Keeps `v` until the process exits. One-shot commands use it for the
/// project and its analysis: freeing a large graph piece by piece takes
/// longer than the rest of the output (20 ms on VS Code), and the OS
/// reclaims it all at once anyway.
fn one_shot<T>(v: T) -> &'static mut T {
    Box::leak(Box::new(v))
}

impl Project {
    /// Prints config notes (once) to stderr.
    fn announce(self) -> Self {
        let p = Paint::stderr();
        for n in &self.notes {
            eprintln!("{}", p.dim(&format!("note: {n}")));
        }
        self
    }
}

/// With `TANGLE_TIMINGS` set, prints how long a phase took to stderr.
pub fn timing(phase: &str, t: std::time::Instant) {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("TANGLE_TIMINGS").is_some());
    if *ON {
        eprintln!("{phase:>8} {:7.1}ms", t.elapsed().as_secs_f64() * 1000.0);
    }
}

fn main() -> ExitCode {
    let t = std::time::Instant::now();
    let code = main_inner();
    timing("total", t);
    code
}

fn main_inner() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e:#}", Paint::stdout().red("error:"));
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let _ = CACHE_ARGS.set((cli.cache.clone(), cli.cache_strategy));
    let p = Paint::stdout();
    match cli.cmd.unwrap_or(Cmd::Tui { target: cli.target, no_watch: false }) {
        Cmd::Tui { target: t, no_watch } => {
            if !std::io::stdout().is_terminal() {
                bail!("the explorer needs a terminal; try `tangle check` or `tangle stats`");
            }
            let mut project = Project::open(&t.path, t.config.as_deref(), t.mode.as_deref())?.announce();
            let a = project.analyze()?;
            let watcher = if no_watch { None } else { watch::Watcher::new(&project.root).ok() };
            tui::run(a, |changes| project.rebuild(changes), watcher)?;
        }
        Cmd::Watch { target: t } => {
            let dir = std::fs::canonicalize(&t.path).with_context(|| format!("{} not found", t.path.display()))?;
            let root = config::find_root(&dir);
            let mut watcher = watch::Watcher::new(&root)?;
            let mut project: Option<Project> = None;
            let mut changes = watch::Changes::full();
            loop {
                // Incremental when we have a project; otherwise (first run, or
                // after a config error) try a full open.
                let result = match project.as_mut() {
                    Some(p) => p.rebuild(&changes).map(|(a, s)| (a, Some(s))),
                    None => Project::open(&t.path, t.config.as_deref(), t.mode.as_deref()).and_then(|p| {
                        let a = p.analyze()?;
                        project = Some(p);
                        Ok((a, None))
                    }),
                };
                print!("\x1b[2J\x1b[3J\x1b[H");
                match result {
                    Ok((a, status)) => {
                        if let Some(s) = status {
                            println!("{}\n", p.dim(&s));
                        }
                        print!("{}", report::text(&a.graph, &a.violations, 0, &report::Stale::none()));
                    }
                    Err(e) => println!("{} {e:#}", p.red("error:")),
                }
                println!("{}", p.dim(&format!("\nwatching {} — ctrl-c to stop", root.display())));
                changes = watcher.wait();
            }
        }
        Cmd::Check { target, format, strict, baseline, write_baseline, baseline_mode } => {
            let project = one_shot(Project::open(&target.path, target.config.as_deref(), target.mode.as_deref())?.announce());
            if let Some(path) = write_baseline {
                let path = path.or_else(|| project.baseline_path()).unwrap_or_else(|| PathBuf::from(".tangle-baseline.json"));
                let a = one_shot(project.analyze_with(false)?);
                let n = rules::write_baseline(&a.graph, &a.violations, &path, baseline_mode == BaselineMode::ShrinkOnly)?;
                eprintln!("wrote {n} violations to {}", path.display());
                return Ok(ExitCode::SUCCESS);
            }
            let a = one_shot(project.analyze()?);
            let mut suppressed = a.suppressed;
            if let Some(b) = &baseline {
                let used = rules::apply_baseline(&a.graph, &mut a.violations, b)?;
                suppressed += used.suppressed;
                a.stale.extend(used.stale);
            }
            let stale = report::Stale { entries: &a.stale, severity: project.cfg.options.baseline_stale };
            let (g, vs) = (&a.graph, &a.violations);
            match format {
                CheckFormat::Text => print!("{}", report::text(g, vs, suppressed, &stale)),
                CheckFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&report::check_json(g, vs, &stale))?)
                }
                CheckFormat::Markdown => print!("{}", report::markdown(g, vs, &stale)),
                CheckFormat::Github | CheckFormat::Teamcity | CheckFormat::Azure => {
                    print!(
                        "{}",
                        match format {
                            CheckFormat::Github => report::github(g, vs, &stale),
                            CheckFormat::Teamcity => report::teamcity(g, vs, &stale),
                            _ => report::azure(g, vs, &stale),
                        }
                    );
                    eprint!("{}", report::text(g, vs, suppressed, &stale));
                }
            }
            let (errors, warnings, _) = report::totals(vs, &stale);
            let failing = errors > 0 || (strict && warnings > 0);
            return Ok(if failing { ExitCode::FAILURE } else { ExitCode::SUCCESS });
        }
        Cmd::Graph { target, format, collapse, focus, focus_depth, reaches, highlight, from, max_depth, externals, no_types, output } => {
            let a = analyze(&target.path, target.config.as_deref(), target.mode.as_deref())?;
            let re = |r: Option<String>, what: &str| {
                r.map(|r| regex::Regex::new(&r).with_context(|| format!("--{what}: invalid regex {r:?}"))).transpose()
            };
            let view = report::GraphView {
                collapse: match collapse {
                    Some(c) => Some(match c.parse::<usize>() {
                        Ok(depth) => report::Collapse::Depth(depth),
                        Err(_) => report::Collapse::Pattern(re(Some(c), "collapse")?.expect("given")),
                    }),
                    None => None,
                },
                focus: re(focus, "focus")?,
                focus_depth,
                reaches: re(reaches, "reaches")?,
                highlight: re(highlight, "highlight")?,
                from: re(from, "from")?,
                max_depth,
                externals,
                type_only: !no_types,
            };
            let text = match format {
                GraphFormat::Dot => report::dot(&a.graph, &view),
                GraphFormat::Mermaid => report::mermaid(&a.graph, &view),
                GraphFormat::D2 => report::d2(&a.graph, &view),
                GraphFormat::Csv => report::csv(&a.graph, &view),
                GraphFormat::Json => serde_json::to_string_pretty(&report::graph_json(&a.graph, &a.violations, &view))? + "\n",
            };
            match output {
                Some(path) => std::fs::write(&path, text)?,
                None => print!("{text}"),
            }
        }
        Cmd::Why { from, to, dir, config, mode } => {
            let a = analyze(&dir, config.as_deref(), mode.as_deref())?;
            let g = &a.graph;
            let (f, t) = (g.lookup(&from).map_err(anyhow::Error::msg)?, g.lookup(&to).map_err(anyhow::Error::msg)?);
            match g.path_between(f, t, false) {
                None => {
                    println!("{} does not depend on {}", g.modules[f].id, g.modules[t].id);
                    return Ok(ExitCode::FAILURE);
                }
                Some(path) => {
                    println!("{}", p.bold(&g.modules[path[0]].id));
                    for w in path.windows(2) {
                        let edge = g.out[w[0]].iter().map(|&e| &g.edges[e]).find(|e| e.to == w[1]).unwrap();
                        let extra: Vec<_> = edge.types.iter().skip(1).copied().collect();
                        println!(
                            "  {} {} {}",
                            p.dim("└─ imports"),
                            p.cyan(&format!("{:?}", edge.specifier)),
                            p.dim(&if extra.is_empty() { String::new() } else { format!("[{}]", extra.join(", ")) })
                        );
                        println!("{}", p.bold(&g.modules[w[1]].id));
                    }
                    println!("{}", p.dim(&format!("{} hops", path.len() - 1)));
                }
            }
        }
        Cmd::Affected { files, since, dir, config, mode, filter } => {
            let a = analyze(&dir, config.as_deref(), mode.as_deref())?;
            let g = &a.graph;
            let mut changed = files;
            if let Some(r) = since {
                let out = std::process::Command::new("git")
                    .args(["diff", "--name-only", "--relative", &r])
                    .current_dir(&g.root)
                    .output()
                    .context("running git")?;
                if !out.status.success() {
                    bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                changed.extend(String::from_utf8_lossy(&out.stdout).lines().map(String::from));
            }
            let starts: Vec<usize> = changed
                .iter()
                .filter_map(|f| g.find(ModuleKind::Local, f.trim_start_matches("./")).or_else(|| g.lookup(f).ok()))
                .collect();
            let filter = filter.map(|f| regex::Regex::new(&f)).transpose()?;
            let mut hit: Vec<&str> = g
                .closure(&starts, false)
                .into_iter()
                .filter(|&m| g.modules[m].kind == ModuleKind::Local)
                .map(|m| g.modules[m].id.as_str())
                .filter(|id| filter.as_ref().is_none_or(|f| f.is_match(id)))
                .collect();
            hit.sort_unstable();
            for id in &hit {
                println!("{id}");
            }
            eprintln!("{}", p.dim(&format!("{} changed → {} affected", starts.len(), hit.len())));
        }
        Cmd::Report { target, output, open } => {
            let a = analyze(&target.path, target.config.as_deref(), target.mode.as_deref())?;
            let html = report::html(&a.graph, &a.violations, a.config_path.as_deref());
            std::fs::write(&output, &html).with_context(|| format!("writing {}", output.display()))?;
            let (e, w, i) = report::counts(&a.violations);
            println!(
                "{} {} ({:.1} MB) · {} modules · {e} errors, {w} warnings, {i} info",
                p.green("wrote"),
                output.display(),
                html.len() as f64 / 1e6,
                a.graph.local_count()
            );
            if open {
                let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
                std::process::Command::new(opener).arg(&output).spawn().context("opening the report")?;
            }
        }
        Cmd::Stats { target, top } => {
            let a = analyze(&target.path, target.config.as_deref(), target.mode.as_deref())?;
            print!("{}", report::stats(&a.graph, &a.violations, top));
        }
        Cmd::Migrate { path, dry_run, force } => {
            let dir = std::fs::canonicalize(&path).with_context(|| format!("{} not found", path.display()))?;
            let root = config::find_root(&dir);
            let sources = migrate::discover(&root);
            if sources.is_empty() {
                println!("No existing dependency rules found in {}.", root.display());
                println!("Looked for: JS/JSON rules configs (forbidden/allowed/required), ESLint import rules, madge.");
                println!("Start from tangle's defaults with `tangle init`, or pass a config: `tangle init --from FILE`.");
                return Ok(ExitCode::SUCCESS);
            }
            let m = migrate::migrate(&root, &sources)?;
            const BASELINE: &str = ".tangle-baseline.json";
            // Always reference the baseline file (ignored while it doesn't exist),
            // so `tangle check --write-baseline` works straight away.
            let text = migrate::render(&m, Some(BASELINE))?;
            // Never write a config tangle can't load.
            let parsed: Config = toml::from_str(&text).context("internal error: generated tangle.toml doesn't parse")?;
            rules::validate(&parsed).context("internal error: generated rules are invalid")?;

            let out = root.join(config::CONFIG_FILE);
            if dry_run {
                print!("{text}");
            } else if out.exists() && !force {
                bail!("{} already exists (use --force to overwrite, or --dry-run to preview)", out.display());
            }
            let e = |s: String| if dry_run { eprintln!("{s}") } else { println!("{s}") };
            e(p.bold("Converted"));
            for (label, summary) in &m.converted {
                e(format!("  {} {label}  {}", p.green("✓"), p.dim(summary)));
            }
            if let Some((from, entries)) = &m.baseline {
                e(format!("  {} {from}  {}", p.green("✓"), p.dim(&format!("{} known violations → {BASELINE}", entries.len()))));
            }
            for label in &m.empty {
                e(format!("  {} {label}  {}", p.dim("·"), p.dim("nothing to convert")));
            }
            if !m.merged.is_empty() {
                e(format!("{} {}", p.bold("Merged duplicates"), p.dim(&format!("({})", m.merged.len()))));
                for w in &m.merged {
                    e(format!("  {w}"));
                }
            }
            if !m.warnings.is_empty() {
                e(p.bold(&format!("Needs review ({}) — also noted at the top of tangle.toml", m.warnings.len())));
                for w in &m.warnings {
                    e(format!("  {} {w}", p.yellow("!")));
                }
            }
            if !m.scripts.is_empty() {
                e(p.bold("Update these package.json scripts"));
                for (name, cmd) in &m.scripts {
                    e(format!("  \"{name}\": {}", p.dim(&format!("{cmd:?}"))));
                    e(format!("  {}  \"{name}\": \"tangle check\"", p.green("→")));
                }
            }
            if dry_run {
                return Ok(ExitCode::SUCCESS);
            }
            std::fs::write(&out, &text)?;
            if let Some((_, entries)) = &m.baseline {
                std::fs::write(root.join(BASELINE), serde_json::to_string_pretty(entries)? + "\n")?;
            }
            println!("{} {}", p.green("wrote"), out.display());
            // Show where things stand right away.
            let a = one_shot(one_shot(Project::open(&root, None, None)?).analyze()?);
            let (err, warn, info) = report::counts(&a.violations);
            println!(
                "{} {err} errors, {warn} warnings, {info} info{} — see them with `tangle check` or `tangle report --open`",
                p.bold("Now:"),
                if a.suppressed > 0 { format!(" ({} baselined)", a.suppressed) } else { String::new() }
            );
            if !a.violations.is_empty() {
                println!(
                    "{}",
                    p.dim(&format!(
                        "To switch CI over without new failures, accept today's findings with `tangle check --write-baseline` (writes {BASELINE}); new violations will still fail."
                    ))
                );
            }
        }
        Cmd::Init { path, from, force } => {
            let file = path.join(config::CONFIG_FILE);
            if file.exists() && !force {
                bail!("{} already exists (use --force to overwrite)", file.display());
            }
            match from {
                Some(src) => {
                    let imported = migrate::import(&src)?;
                    std::fs::write(&file, migrate::to_toml(&imported, &src)?)?;
                    let c = &imported.config;
                    println!(
                        "{} {} from {} ({} forbidden, {} allowed, {} required)",
                        p.green("created"),
                        file.display(),
                        src.display(),
                        c.forbidden.len(),
                        c.allowed.len(),
                        c.required.len()
                    );
                    for w in &imported.warnings {
                        println!("  {} {w}", p.yellow("not converted:"));
                    }
                }
                None => {
                    let existing = migrate::discover(&config::find_root(&std::fs::canonicalize(&path)?));
                    if !existing.is_empty() && !force {
                        let names: Vec<String> = existing.iter().map(|s| s.label(&path)).collect();
                        println!("Found existing dependency rules: {}", names.join(", "));
                        println!("Run `tangle migrate` to convert them (or `tangle init --force` for the defaults).");
                        return Ok(ExitCode::SUCCESS);
                    }
                    std::fs::write(&file, config::DEFAULT_CONFIG)?;
                    println!("{} {}", p.green("created"), file.display());
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
