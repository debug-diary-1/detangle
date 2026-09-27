mod config;
mod graph;
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

use crate::config::{Config, Severity};
use crate::graph::{Graph, ModuleKind};
use crate::report::Paint;
use crate::rules::Violation;

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
}

#[derive(Args, Clone)]
struct Target {
    /// Directory to analyse (the project root is found by walking up to tangle.toml / package.json)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Config file (default: <root>/tangle.toml, else built-in rules)
    #[arg(short, long)]
    config: Option<PathBuf>,
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
        /// Record all current violations to this baseline file and exit 0
        #[arg(long)]
        write_baseline: Option<PathBuf>,
    },
    /// Export the dependency graph (dot, mermaid, json)
    Graph {
        #[command(flatten)]
        target: Target,
        #[arg(short, long, value_enum, default_value_t = GraphFormat::Dot)]
        format: GraphFormat,
        /// Collapse local modules to their first N path segments (e.g. 2 → src/feature/)
        #[arg(long)]
        collapse: Option<usize>,
        /// Only show modules matching this regex, plus their direct neighbours
        #[arg(long)]
        focus: Option<String>,
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
    Github,
}

#[derive(Clone, Copy, ValueEnum)]
enum GraphFormat {
    Dot,
    Mermaid,
    Json,
}

pub struct Analysis {
    pub graph: Graph,
    pub violations: Vec<Violation>,
    pub config_path: Option<PathBuf>,
}

/// A loaded project whose scan can be kept up to date incrementally.
pub struct Project {
    dir: PathBuf,
    root: PathBuf,
    config_arg: Option<PathBuf>,
    cfg: Config,
    config_path: Option<PathBuf>,
    /// Notes from loading the config (e.g. JavaScript config import warnings).
    notes: Vec<String>,
    session: scan::Session,
}

impl Project {
    fn open(path: &Path, config: Option<&Path>) -> Result<Self> {
        let dir = std::fs::canonicalize(path).with_context(|| format!("{} not found", path.display()))?;
        if !dir.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        let root = config::find_root(&dir);
        let loaded = config::load(&root, config)?;
        let cfg: Config = loaded.config;
        rules::validate(&cfg).with_context(|| match &loaded.path {
            Some(p) => format!("in {}", p.display()),
            None => "in the built-in rules".into(),
        })?;
        let session = scan::Session::new(&root, &dir, &cfg.options)?;
        Ok(Project {
            dir,
            root,
            config_arg: config.map(Path::to_path_buf),
            cfg,
            config_path: loaded.path,
            notes: loaded.notes,
            session,
        })
    }

    fn analyze(&self) -> Result<Analysis> {
        let graph = Graph::build(&self.root, self.session.files(), self.session.work, &self.cfg.options);
        let violations = rules::evaluate(&graph, &self.cfg)?;
        Ok(Analysis { graph, violations, config_path: self.config_path.clone() })
    }

    /// Applies filesystem changes and re-analyses. Returns the new analysis
    /// and a one-line description of the work done. On error (e.g. a
    /// half-edited tangle.toml) the previous state is kept.
    fn rebuild(&mut self, changes: &watch::Changes) -> Result<(Analysis, String)> {
        let t = std::time::Instant::now();
        if changes.config {
            *self = Project::open(&self.dir, self.config_arg.as_deref())?;
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

fn analyze(path: &Path, config: Option<&Path>) -> Result<Analysis> {
    Project::open(path, config)?.announce().analyze()
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

fn main() -> ExitCode {
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
    let p = Paint::stdout();
    match cli.cmd.unwrap_or(Cmd::Tui { target: cli.target, no_watch: false }) {
        Cmd::Tui { target: t, no_watch } => {
            if !std::io::stdout().is_terminal() {
                bail!("the explorer needs a terminal; try `tangle check` or `tangle stats`");
            }
            let mut project = Project::open(&t.path, t.config.as_deref())?.announce();
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
                    None => Project::open(&t.path, t.config.as_deref()).and_then(|p| {
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
                        print!("{}", report::text(&a.graph, &a.violations, 0));
                    }
                    Err(e) => println!("{} {e:#}", p.red("error:")),
                }
                println!("{}", p.dim(&format!("\nwatching {} — ctrl-c to stop", root.display())));
                changes = watcher.wait();
            }
        }
        Cmd::Check { target, format, strict, baseline, write_baseline } => {
            let mut a = analyze(&target.path, target.config.as_deref())?;
            if let Some(path) = write_baseline {
                rules::write_baseline(&a.graph, &a.violations, &path)?;
                eprintln!("wrote {} violations to {}", a.violations.len(), path.display());
                return Ok(ExitCode::SUCCESS);
            }
            let suppressed = match &baseline {
                Some(b) => rules::apply_baseline(&a.graph, &mut a.violations, b)?,
                None => 0,
            };
            match format {
                CheckFormat::Text => print!("{}", report::text(&a.graph, &a.violations, suppressed)),
                CheckFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&report::violations_json(&a.graph, &a.violations))?
                ),
                CheckFormat::Github => {
                    print!("{}", report::github(&a.graph, &a.violations));
                    eprint!("{}", report::text(&a.graph, &a.violations, suppressed));
                }
            }
            let failing = a.violations.iter().any(|v| {
                v.severity == Severity::Error || (strict && v.severity == Severity::Warn)
            });
            return Ok(if failing { ExitCode::FAILURE } else { ExitCode::SUCCESS });
        }
        Cmd::Graph { target, format, collapse, focus, externals, no_types, output } => {
            let a = analyze(&target.path, target.config.as_deref())?;
            let view = report::GraphView {
                collapse,
                focus: focus.map(|f| regex::Regex::new(&f)).transpose()?,
                externals,
                type_only: !no_types,
            };
            let text = match format {
                GraphFormat::Dot => report::dot(&a.graph, &view),
                GraphFormat::Mermaid => report::mermaid(&a.graph, &view),
                GraphFormat::Json => {
                    serde_json::to_string_pretty(&report::full_json(&a.graph, &a.violations))? + "\n"
                }
            };
            match output {
                Some(path) => std::fs::write(&path, text)?,
                None => print!("{text}"),
            }
        }
        Cmd::Why { from, to, dir, config } => {
            let a = analyze(&dir, config.as_deref())?;
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
        Cmd::Affected { files, since, dir, config, filter } => {
            let a = analyze(&dir, config.as_deref())?;
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
            let a = analyze(&target.path, target.config.as_deref())?;
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
            let a = analyze(&target.path, target.config.as_deref())?;
            print!("{}", report::stats(&a.graph, &a.violations, top));
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
                    std::fs::write(&file, config::DEFAULT_CONFIG)?;
                    println!("{} {}", p.green("created"), file.display());
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
