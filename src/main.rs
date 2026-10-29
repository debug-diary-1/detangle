mod affected;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use detangle::config::{self, Config};
use detangle::graph::ModuleKind;
use detangle::report::{self, Paint};
use detangle::{Analysis, CacheArgs, Project, migrate, rules, timing, tui, watch};

// Parsing and graph building allocate heavily from many threads, where
// mimalloc is much faster than the system allocator (notably on macOS).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "detangle",
    version,
    about = "Blazing-fast dependency analysis and architecture rules for JS/TS projects",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    target: Target,
    /// Keep parse results between runs (default dir: node_modules/.cache/detangle)
    #[arg(long, global = true, num_args = 0..=1, value_name = "DIR")]
    cache: Option<Option<PathBuf>>,
    /// How the cache detects changed files
    #[arg(long, global = true, value_enum)]
    cache_strategy: Option<config::CacheStrategy>,
}

#[derive(Args, Clone)]
struct Target {
    /// Directory to analyse (the project root is found by walking up to detangle.toml / package.json)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Config file (default: <root>/detangle.toml, else built-in rules)
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
        /// file: options.baseline, else .detangle-baseline.json)
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
        /// Explain each result with one shortest import path to a changed module
        #[arg(long)]
        why: bool,
        /// Output format (JSON always includes explanations)
        #[arg(short, long, value_enum, default_value_t = AffectedFormat::Text)]
        format: AffectedFormat,
    },
    /// Write a self-contained HTML report (violations, modules, cycles, folder graph)
    Report {
        #[command(flatten)]
        target: Target,
        /// Output file
        #[arg(short, long, default_value = "detangle-report.html")]
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
    /// rules, madge) and known-violation files into detangle.toml
    Migrate {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the detangle.toml instead of writing it
        #[arg(long)]
        dry_run: bool,
        /// Overwrite an existing detangle.toml
        #[arg(long)]
        force: bool,
    },
    /// Write a starter detangle.toml
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Convert a JavaScript rules config (.js/.cjs/.mjs/.json) to detangle.toml
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AffectedFormat {
    Text,
    Json,
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

/// Analyses a project for a command that exits afterwards.
fn analyze(path: &Path, config: Option<&Path>, mode: Option<&str>, cache: &CacheArgs) -> Result<&'static mut Analysis> {
    let project = one_shot(announce(Project::open(path, config, mode, cache)?));
    Ok(one_shot(project.analyze()?))
}

/// Files changed on this branch, relative to `root`: whatever differs
/// between the merge-base of `since` and HEAD and the working tree
/// (committed, staged and unstaged), plus untracked files git doesn't ignore.
/// Comparing from the merge-base keeps out commits that landed on `since`
/// after the branch. Comparison identities are resolved once for this discovery.
struct GitChanges {
    present: Vec<String>,
    deleted: Vec<String>,
    base: String,
    head: String,
}

fn changed_since(root: &Path, since: &str, strict_paths: bool) -> Result<GitChanges> {
    let git = |args: &[&str]| -> Result<Vec<u8>> {
        let out = std::process::Command::new("git").args(args).current_dir(root).output().context("running git")?;
        if !out.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    };
    if git(&["rev-parse", "--git-dir"]).is_err() {
        bail!("--since {since}: {} isn't in a git repository", root.display());
    }
    if git(&["rev-parse", "--verify", "--quiet", &format!("{since}^{{commit}}")]).is_err() {
        bail!("--since {since}: not a commit in this repository; fetch it first (with actions/checkout, `fetch-depth: 0`)");
    }
    let head = String::from_utf8(git(&["rev-parse", "--verify", "HEAD"])?).context("reading Git HEAD")?.trim().to_owned();
    let base = match git(&["merge-base", since, &head]) {
        Ok(b) if !b.is_empty() => String::from_utf8_lossy(&b).trim().to_string(),
        _ => bail!(
            "--since {since}: no common ancestor with HEAD; in a shallow clone, fetch more history (with actions/checkout, `fetch-depth: 0`)"
        ),
    };
    let fields = |bytes: Vec<u8>| -> Result<Vec<String>> {
        bytes.split(|&b| b == 0).filter(|f| !f.is_empty()).map(|f| {
            if strict_paths {
                std::str::from_utf8(f).map(str::to_owned).context(
                    "unsupported Git path encoding: affected explanations require UTF-8 paths; rename the path or use default text output"
                )
            } else {
                Ok(String::from_utf8_lossy(f).into_owned())
            }
        }).collect()
    };
    // `--no-renames`: a rename is its old path deleted and its new path added.
    let diff = fields(git(&["diff", "--name-status", "-z", "--no-renames", "--relative", &base])?)?;
    let (mut present, mut deleted) = (vec![], vec![]);
    for pair in diff.chunks(2) {
        if let [status, path] = pair {
            if status.starts_with('D') { deleted.push(path.clone()) } else { present.push(path.clone()) }
        }
    }
    present.extend(fields(git(&["ls-files", "--others", "--exclude-standard", "-z"])?)?);
    Ok(GitChanges { present, deleted, base, head })
}

/// Keeps `v` until the process exits. One-shot commands use it for the
/// project and its analysis: freeing a large graph piece by piece takes
/// longer than the rest of the output (20 ms on VS Code), and the OS
/// reclaims it all at once anyway.
fn one_shot<T>(v: T) -> &'static mut T {
    Box::leak(Box::new(v))
}

/// Prints a project's config notes to stderr.
fn announce(project: Project) -> Project {
    let p = Paint::stderr();
    for n in project.notes() {
        eprintln!("{}", p.dim(&format!("note: {n}")));
    }
    project
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
    let cache = CacheArgs { cache: cli.cache, strategy: cli.cache_strategy };
    let p = Paint::stdout();
    match cli.cmd.unwrap_or(Cmd::Tui { target: cli.target, no_watch: false }) {
        Cmd::Tui { target: t, no_watch } => {
            if !std::io::stdout().is_terminal() {
                bail!("the explorer needs a terminal; try `detangle check` or `detangle stats`");
            }
            let mut project = announce(Project::open(&t.path, t.config.as_deref(), t.mode.as_deref(), &cache)?);
            let a = project.analyze()?;
            let watcher = if no_watch { None } else { watch::Watcher::new(project.root()).ok() };
            tui::run(a, |changes| project.rebuild(changes), watcher)?;
        }
        Cmd::Watch { target: t } => {
            let dir = dunce::canonicalize(&t.path).with_context(|| format!("{} not found", t.path.display()))?;
            let root = config::find_root(&dir);
            let mut watcher = watch::Watcher::new(&root)?;
            let mut project: Option<Project> = None;
            // The analysis on screen, kept while edits don't change imports.
            let mut shown: Option<Analysis> = None;
            let mut changes = watch::Changes::full();
            loop {
                // Incremental when we have a project; otherwise (first run, or
                // after a config error) try a full open.
                let result = match project.as_mut() {
                    Some(p) => p.rebuild(&changes).map(|(a, s)| (a, Some(s))),
                    None => Project::open(&t.path, t.config.as_deref(), t.mode.as_deref(), &cache).and_then(|p| {
                        let a = p.analyze()?;
                        project = Some(p);
                        Ok((Some(a), None))
                    }),
                };
                print!("\x1b[2J\x1b[3J\x1b[H");
                match result {
                    Ok((a, status)) => {
                        if let Some(s) = status {
                            println!("{}\n", p.dim(&s));
                        }
                        if a.is_some() {
                            shown = a;
                        }
                        if let Some(a) = &shown {
                            print!("{}", report::text(&a.graph, &a.violations, 0, &report::Stale::none()));
                        }
                    }
                    Err(e) => println!("{} {e:#}", p.red("error:")),
                }
                println!("{}", p.dim(&format!("\nwatching {} — ctrl-c to stop", root.display())));
                changes = watcher.wait();
            }
        }
        Cmd::Check { target, format, strict, baseline, write_baseline, baseline_mode } => {
            let project = one_shot(announce(Project::open(&target.path, target.config.as_deref(), target.mode.as_deref(), &cache)?));
            if let Some(path) = write_baseline {
                let path = path.or_else(|| project.baseline_path()).unwrap_or_else(|| PathBuf::from(".detangle-baseline.json"));
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
            let stale = report::Stale { entries: &a.stale, severity: project.config().options.baseline_stale };
            let (g, vs) = (&a.graph, &a.violations);
            let not_installed = report::not_installed_note(g);
            if let Some(n) = &not_installed {
                eprintln!("{}", Paint::stderr().dim(&format!("note: {n}")));
            }
            match format {
                CheckFormat::Text => print!("{}", report::text(g, vs, suppressed, &stale)),
                CheckFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&report::check_json(g, vs, &stale))?)
                }
                CheckFormat::Markdown => print!("{}", report::markdown(g, vs, &stale)),
                CheckFormat::Github | CheckFormat::Teamcity | CheckFormat::Azure => {
                    if let (CheckFormat::Github, Some(n)) = (format, &not_installed) {
                        println!("::warning title=detangle::{n}");
                    }
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
            let a = analyze(&target.path, target.config.as_deref(), target.mode.as_deref(), &cache)?;
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
            let a = analyze(&dir, config.as_deref(), mode.as_deref(), &cache)?;
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
        Cmd::Affected { files, since, dir, config, mode, filter, why, format } => {
            let project = one_shot(announce(Project::open(&dir, config.as_deref(), mode.as_deref(), &cache)?));
            let a = one_shot(project.analyze()?);
            let g = &a.graph;
            let explained = why || format == AffectedFormat::Json;
            let mut inputs = std::collections::BTreeMap::new();
            if format == AffectedFormat::Json {
                for f in &files {
                    inputs.entry(f.clone()).or_insert_with(affected::Input::default).origins.explicit = true;
                }
            }
            let (mut present, mut deleted) = (files, vec![]);
            let mut comparison = affected::Comparison { mode: "explicit", requested_reference: None, resolved_base: None, head: None };
            let git_changes = since.as_deref().map(|r| changed_since(&g.root, r, explained)).transpose()?;
            if let Some(changes) = &git_changes {
                comparison = affected::Comparison {
                    mode: "merge-base-to-worktree", requested_reference: since.as_deref(),
                    resolved_base: Some(&changes.base), head: Some(&changes.head),
                };
                if format == AffectedFormat::Json {
                    for f in &changes.present {
                        inputs.entry(f.clone()).or_insert_with(affected::Input::default).origins.git = true;
                    }
                    for f in &changes.deleted {
                        let input = inputs.entry(f.clone()).or_insert_with(affected::Input::default);
                        input.origins.git = true;
                        input.deleted = true;
                        input.classification = affected::Classification::Deleted;
                    }
                }
                present.extend(changes.present.iter().cloned());
                deleted.clone_from(&changes.deleted);
            }
            present.sort();
            present.dedup();
            let is_config = |f: &str| {
                let name = f.rsplit('/').next().unwrap_or(f);
                watch::is_config(name) || detangle::stamps::LOCKFILES.contains(&name)
            };
            let (config_changed, present): (Vec<String>, Vec<String>) = present.into_iter().partition(|f| is_config(f));
            let all_deleted = if explained { deleted.clone() } else { vec![] };
            let (config_deleted, deleted): (Vec<String>, Vec<String>) = deleted.into_iter().partition(|f| is_config(f));
            let config_changed: Vec<String> = config_changed.into_iter().chain(config_deleted).collect();
            let mut starts = vec![];
            let mut unmatched = vec![];
            for f in &present {
                match g.find(ModuleKind::Local, f.trim_start_matches("./")).or_else(|| g.lookup(f).ok()) {
                    Some(m) => {
                        starts.push(m);
                        if let Some(input) = inputs.get_mut(f) {
                            if !input.deleted { input.classification = affected::Classification::Module; }
                            input.module = Some(g.modules[m].id.clone());
                        }
                    },
                    None => unmatched.push(f.clone()),
                }
            }
            starts.sort_unstable();
            starts.dedup();
            for f in &config_changed {
                if let Some(input) = inputs.get_mut(f) { input.classification = affected::Classification::Configuration; }
            }
            let filter_text = filter.clone();
            let filter = filter.map(|f| regex::Regex::new(&f)).transpose()?;
            let evidence = explained.then(|| affected::Evidence::build(g, &starts));
            let reached: Vec<usize> = match &evidence {
                Some(e) => e.reached.keys().copied().collect(),
                None => g.closure(&starts, false).into_iter().collect(),
            };
            let total_affected = reached.iter().filter(|&&m| g.modules[m].kind == ModuleKind::Local).count();
            let mut hit: Vec<usize> = reached.into_iter()
                .filter(|&m| g.modules[m].kind == ModuleKind::Local)
                .filter(|&m| filter.as_ref().is_none_or(|f| f.is_match(&g.modules[m].id)))
                .collect();
            hit.sort_unstable_by_key(|&m| &g.modules[m].id);
            let limitations = explained.then(|| affected::limitations(g, &project.config().options, since.is_some(), &all_deleted, &config_changed, &unmatched));
            if format == AffectedFormat::Json {
                affected::write_json(g, evidence.as_ref().unwrap(), &hit, affected::Report {
                    schema_version: 1,
                    comparison,
                    scope: affected::Scope {
                        root: &g.root, basis: "current-graph",
                        graph_restrictions: affected::graph_restrictions(&project.config().options).collect(),
                        output_filter: filter_text.as_deref(),
                    },
                    counts: affected::Counts { inputs: inputs.len(), seeds: starts.len(), affected: total_affected, displayed: hit.len() },
                    inputs: inputs.iter().map(|(path, input)| affected::ReportedInput { path, input }).collect(),
                    limitations: limitations.as_deref().unwrap(),
                })?;
            } else {
                for &m in &hit {
                    match &evidence {
                        Some(e) => e.print_path(g, m),
                        None => println!("{}", g.modules[m].id),
                    }
                }
            }
            let mut summary = format!("{} changed {}", starts.len(), if starts.len() == 1 { "module" } else { "modules" });
            for (n, what) in [(deleted.len(), "deleted"), (config_changed.len(), "config"), (unmatched.len(), "other")] {
                if n > 0 {
                    summary += &format!(", {n} {what}");
                }
            }
            eprintln!("{}", p.dim(&format!("{summary} → {} affected", hit.len())));
            let list = |v: &[String]| {
                let mut s = v.iter().take(5).map(|s| if explained { affected::display_path(s) } else { s.clone() }).collect::<Vec<_>>().join(", ");
                if v.len() > 5 {
                    s += &format!(", and {} more", v.len() - 5);
                }
                s
            };
            if !deleted.is_empty() {
                eprintln!("{}", p.dim(&format!("deleted: {} (files that still import them aren't found)", list(&deleted))));
            }
            if !config_changed.is_empty() {
                eprintln!("{}", p.dim(&format!("config changed: {} (this can affect any module; not followed)", list(&config_changed))));
            }
            if let Some(limitations) = limitations {
                affected::print_scope(g, &project.config().options, filter_text.as_deref(), &limitations);
            }
        }
        Cmd::Report { target, output, open } => {
            let a = analyze(&target.path, target.config.as_deref(), target.mode.as_deref(), &cache)?;
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
            let a = analyze(&target.path, target.config.as_deref(), target.mode.as_deref(), &cache)?;
            print!("{}", report::stats(&a.graph, &a.violations, top));
        }
        Cmd::Migrate { path, dry_run, force } => {
            let dir = dunce::canonicalize(&path).with_context(|| format!("{} not found", path.display()))?;
            let root = config::find_root(&dir);
            let sources = migrate::discover(&root);
            if sources.is_empty() {
                println!("No existing dependency rules found in {}.", root.display());
                println!("Looked for: JS/JSON rules configs (forbidden/allowed/required), ESLint import rules, madge.");
                println!("Start from detangle's defaults with `detangle init`, or pass a config: `detangle init --from FILE`.");
                return Ok(ExitCode::SUCCESS);
            }
            let m = migrate::migrate(&root, &sources)?;
            const BASELINE: &str = ".detangle-baseline.json";
            // Always reference the baseline file (ignored while it doesn't exist),
            // so `detangle check --write-baseline` works straight away.
            let text = migrate::render(&m, Some(BASELINE))?;
            // Never write a config detangle can't load.
            let parsed: Config = toml::from_str(&text).context("internal error: generated detangle.toml doesn't parse")?;
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
                e(p.bold(&format!("Needs review ({}) — also noted at the top of detangle.toml", m.warnings.len())));
                for w in &m.warnings {
                    e(format!("  {} {w}", p.yellow("!")));
                }
            }
            if !m.scripts.is_empty() {
                e(p.bold("Update these package.json scripts"));
                for (name, cmd) in &m.scripts {
                    e(format!("  \"{name}\": {}", p.dim(&format!("{cmd:?}"))));
                    e(format!("  {}  \"{name}\": \"detangle check\"", p.green("→")));
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
            let a = one_shot(one_shot(Project::open(&root, None, None, &cache)?).analyze()?);
            let (err, warn, info) = report::counts(&a.violations);
            println!(
                "{} {err} errors, {warn} warnings, {info} info{} — see them with `detangle check` or `detangle report --open`",
                p.bold("Now:"),
                if a.suppressed > 0 { format!(" ({} baselined)", a.suppressed) } else { String::new() }
            );
            if !a.violations.is_empty() {
                println!(
                    "{}",
                    p.dim(&format!(
                        "To switch CI over without new failures, accept today's findings with `detangle check --write-baseline` (writes {BASELINE}); new violations will still fail."
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
                    let existing = migrate::discover(&config::find_root(&dunce::canonicalize(&path)?));
                    if !existing.is_empty() && !force {
                        let names: Vec<String> = existing.iter().map(|s| s.label(&path)).collect();
                        println!("Found existing dependency rules: {}", names.join(", "));
                        println!("Run `detangle migrate` to convert them (or `detangle init --force` for the defaults).");
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
