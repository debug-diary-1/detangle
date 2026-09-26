mod config;
mod graph;
mod report;
mod rules;
mod scan;
mod tui;

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
    /// Explore the dependency graph interactively (default)
    Tui(Target),
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

fn analyze(path: &Path, config: Option<&Path>) -> Result<Analysis> {
    let dir = std::fs::canonicalize(path).with_context(|| format!("{} not found", path.display()))?;
    if !dir.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let root = config::find_root(&dir);
    let (cfg, config_path): (Config, _) = config::load(&root, config)?;
    rules::validate(&cfg.forbidden)?;
    let scan = scan::scan(&root, &dir, &cfg.options)?;
    let graph = Graph::build(&root, scan, &cfg.options);
    let violations = rules::evaluate(&graph, &cfg.forbidden)?;
    Ok(Analysis { graph, violations, config_path })
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
    match cli.cmd.unwrap_or(Cmd::Tui(cli.target)) {
        Cmd::Tui(t) => {
            if !std::io::stdout().is_terminal() {
                bail!("the explorer needs a terminal; try `tangle check` or `tangle stats`");
            }
            let a = analyze(&t.path, t.config.as_deref())?;
            tui::run(&a)?;
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
        Cmd::Stats { target, top } => {
            let a = analyze(&target.path, target.config.as_deref())?;
            print!("{}", report::stats(&a.graph, &a.violations, top));
        }
        Cmd::Init { path, force } => {
            let file = path.join(config::CONFIG_FILE);
            if file.exists() && !force {
                bail!("{} already exists (use --force to overwrite)", file.display());
            }
            std::fs::write(&file, config::DEFAULT_CONFIG)?;
            println!("{} {}", p.green("created"), file.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}
