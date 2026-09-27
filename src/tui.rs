//! Interactive explorer.

use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::Analysis;
use crate::config::{Scope, Severity};
use crate::graph::{Edge, Graph, ModuleKind};
use crate::watch::{Changes, Watcher};

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Modules,
    Violations,
    Cycles,
    Hotspots,
}

const TABS: [Tab; 4] = [Tab::Modules, Tab::Violations, Tab::Cycles, Tab::Hotspots];

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    List,
    Deps,
    Dependents,
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Path,
    FanIn,
    FanOut,
    Instability,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Path => Sort::FanIn,
            Sort::FanIn => Sort::FanOut,
            Sort::FanOut => Sort::Instability,
            Sort::Instability => Sort::Path,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Path => "path",
            Sort::FanIn => "fan-in",
            Sort::FanOut => "fan-out",
            Sort::Instability => "instability",
        }
    }
}

const ACCENT: Color = Color::Cyan;
const PAGE: isize = 15;

struct App<'a> {
    a: &'a Analysis,
    g: &'a Graph,
    /// Violation indices touching each module (as `from` or `to`).
    viol_of: Vec<Vec<usize>>,
    worst: Vec<Option<Severity>>,
    tab: Tab,
    pane: Pane,
    sort: Sort,
    show_ext: bool,
    filter: String,
    editing: bool,
    help: bool,
    quit: bool,
    rows: Vec<usize>,
    list: ListState,
    deps: ListState,
    rdeps: ListState,
    vlist: ListState,
    clist: ListState,
    hot: [ListState; 3],
    hot_col: usize,
    hot_rows: [Vec<usize>; 3],
    cycle_paths: Vec<Vec<usize>>,
    history: Vec<usize>,
    live: bool,
    reload: bool,
    status: Option<String>,
}

/// UI state that survives a reload; modules are keyed by id since indices change.
struct Snapshot {
    tab: Tab,
    pane: Pane,
    sort: Sort,
    show_ext: bool,
    filter: String,
    selected: Option<(ModuleKind, String)>,
    history: Vec<(ModuleKind, String)>,
    vsel: Option<usize>,
    csel: Option<usize>,
    hot_col: usize,
}

enum Outcome {
    Quit,
    Reload(Changes),
}

/// Runs the explorer. With a watcher, the graph is rebuilt whenever relevant
/// files change; `r` forces a rebuild either way.
pub fn run(
    initial: Analysis,
    mut rebuild: impl FnMut(&Changes) -> Result<(Analysis, String)>,
    mut watcher: Option<Watcher>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = (|| -> Result<()> {
        let mut analysis = initial;
        let mut snapshot: Option<Snapshot> = None;
        let mut status: Option<String> = None;
        loop {
            let mut app = App::new(&analysis);
            app.live = watcher.is_some();
            app.status = status.take();
            if let Some(s) = snapshot.take() {
                app.restore(s);
            }
            let changed = match app.event_loop(&mut terminal, &mut watcher)? {
                Outcome::Quit => return Ok(()),
                Outcome::Reload(changed) => changed,
            };
            snapshot = Some(app.snapshot());
            drop(app);
            status = Some(match rebuild(&changed) {
                Ok((a, summary)) => {
                    analysis = a;
                    summary
                }
                Err(e) => format!("✖ reload failed: {e:#}"),
            });
        }
    })();
    ratatui::restore();
    res
}

fn select_first(len: usize) -> ListState {
    ListState::default().with_selected(if len > 0 { Some(0) } else { None })
}

fn mv(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    state.select(Some((cur + delta).clamp(0, len as isize - 1) as usize));
}

fn kind_color(k: ModuleKind) -> Color {
    match k {
        ModuleKind::Local => Color::Reset,
        ModuleKind::Npm => Color::Cyan,
        ModuleKind::Builtin => Color::Green,
        ModuleKind::Unresolved => Color::Red,
    }
}

fn sev_style(s: Severity) -> Style {
    match s {
        Severity::Error => Style::new().fg(Color::Red).bold(),
        Severity::Warn => Style::new().fg(Color::Yellow).bold(),
        Severity::Info => Style::new().fg(Color::Blue),
        Severity::Off => Style::new().dim(),
    }
}

fn block(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { Style::new().fg(ACCENT) } else { Style::new().fg(Color::DarkGray) })
        .title(title)
}

fn list_widget<'a>(items: Vec<ListItem<'a>>, b: Block<'a>, focused: bool) -> List<'a> {
    let hl = if focused {
        Style::new().bg(Color::Rgb(40, 60, 80)).bold()
    } else {
        Style::new().bg(Color::Rgb(40, 40, 40))
    };
    List::new(items).block(b).highlight_style(hl).highlight_symbol("▌")
}

/// `src/foo/bar.ts` with the directory dimmed and the file name emphasised.
fn path_spans(id: &str, kind: ModuleKind) -> Vec<Span<'static>> {
    if kind != ModuleKind::Local {
        return vec![Span::styled(id.to_string(), Style::new().fg(kind_color(kind)))];
    }
    match id.rfind('/') {
        Some(i) => vec![
            Span::styled(id[..=i].to_string(), Style::new().fg(Color::Gray).dim()),
            Span::raw(id[i + 1..].to_string()),
        ],
        None => vec![Span::raw(id.to_string())],
    }
}

fn edge_tags(e: &Edge) -> Vec<Span<'static>> {
    let mut v = vec![];
    for t in &e.types {
        let (label, color) = match *t {
            "npm-dev" => ("dev", Color::Magenta),
            "npm-peer" => ("peer", Color::Magenta),
            "npm-optional" => ("optional", Color::Magenta),
            "npm-undeclared" => ("undeclared", Color::Red),
            "type-only" => ("type", Color::Blue),
            "dynamic" => ("dynamic", Color::Yellow),
            "require" => ("cjs", Color::Yellow),
            "reexport" => ("re-export", Color::DarkGray),
            "resource" => ("resource", Color::DarkGray),
            _ => continue,
        };
        v.push(Span::raw(" "));
        v.push(Span::styled(format!("[{label}]"), Style::new().fg(color)));
    }
    v
}

impl<'a> App<'a> {
    fn new(a: &'a Analysis) -> Self {
        let g = &a.graph;
        let n = g.modules.len();
        let mut viol_of = vec![vec![]; n];
        let mut worst: Vec<Option<Severity>> = vec![None; n];
        // Folder violations are attributed to every module in the folder's subtree.
        let mut by_folder: rustc_hash::FxHashMap<String, Vec<usize>> = Default::default();
        if a.violations.iter().any(|v| v.scope == Scope::Folder) {
            for m in 0..n {
                let Some(mut f) = g.folder_of(m) else { continue };
                loop {
                    by_folder.entry(f.clone()).or_default().push(m);
                    match f.rfind('/') {
                        Some(i) => f.truncate(i),
                        None => break,
                    }
                }
            }
        }
        for (i, v) in a.violations.iter().enumerate() {
            let touched: Vec<usize> = match v.scope {
                Scope::Module => std::iter::once(v.from).chain(v.to).collect(),
                Scope::Folder => std::iter::once(v.source_id(g))
                    .chain(v.target_id(g))
                    .flat_map(|f| by_folder.get(f).cloned().unwrap_or_default())
                    .collect(),
            };
            for m in touched {
                viol_of[m].push(i);
                worst[m] = worst[m].max(Some(v.severity));
            }
        }
        let rank = |key: &dyn Fn(usize) -> usize, kind: ModuleKind| {
            let mut v: Vec<usize> = (0..n).filter(|&m| g.modules[m].kind == kind && key(m) > 0).collect();
            v.sort_by(|&x, &y| key(y).cmp(&key(x)).then_with(|| g.modules[x].id.cmp(&g.modules[y].id)));
            v.truncate(200);
            v
        };
        let hot_rows = [
            rank(&|m| g.fan_in(m), ModuleKind::Local),
            rank(&|m| g.fan_out(m), ModuleKind::Local),
            rank(&|m| g.fan_in(m), ModuleKind::Npm),
        ];
        let cycle_paths = (0..g.cycles.len()).map(|c| g.representative_cycle(c)).collect();
        let mut app = App {
            a,
            g,
            viol_of,
            worst,
            tab: Tab::Modules,
            pane: Pane::List,
            sort: Sort::Path,
            show_ext: false,
            filter: String::new(),
            editing: false,
            help: false,
            quit: false,
            rows: vec![],
            list: ListState::default(),
            deps: ListState::default(),
            rdeps: ListState::default(),
            vlist: select_first(a.violations.len()),
            clist: select_first(g.cycles.len()),
            hot: [select_first(hot_rows[0].len()), select_first(hot_rows[1].len()), select_first(hot_rows[2].len())],
            hot_col: 0,
            hot_rows,
            cycle_paths,
            history: vec![],
            live: false,
            reload: false,
            status: None,
        };
        app.rebuild();
        app
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal, watcher: &mut Option<Watcher>) -> Result<Outcome> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            if self.quit {
                return Ok(Outcome::Quit);
            }
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                self.on_key(k);
            }
            if std::mem::take(&mut self.reload) {
                return Ok(Outcome::Reload(Changes::full()));
            }
            if let Some(changed) = watcher.as_mut().and_then(Watcher::poll) {
                return Ok(Outcome::Reload(changed));
            }
        }
    }

    fn key_of(&self, m: usize) -> (ModuleKind, String) {
        (self.g.modules[m].kind, self.g.modules[m].id.clone())
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            tab: self.tab,
            pane: self.pane,
            sort: self.sort,
            show_ext: self.show_ext,
            filter: self.filter.clone(),
            selected: self.current().map(|m| self.key_of(m)),
            history: self.history.iter().map(|&m| self.key_of(m)).collect(),
            vsel: self.vlist.selected(),
            csel: self.clist.selected(),
            hot_col: self.hot_col,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        let g = self.g;
        let find = |(k, id): &(ModuleKind, String)| g.find(*k, id);
        self.pane = s.pane;
        self.sort = s.sort;
        self.show_ext = s.show_ext;
        self.filter = s.filter;
        self.history = s.history.iter().filter_map(find).collect();
        self.rebuild();
        if let Some(m) = s.selected.as_ref().and_then(find)
            && let Some(pos) = self.rows.iter().position(|&r| r == m)
        {
            self.list.select(Some(pos));
            self.reset_side();
        }
        let clamp = |sel: Option<usize>, len: usize| if len == 0 { None } else { Some(sel.unwrap_or(0).min(len - 1)) };
        self.vlist.select(clamp(s.vsel, self.a.violations.len()));
        self.clist.select(clamp(s.csel, g.cycles.len()));
        self.hot_col = s.hot_col;
        self.tab = s.tab;
    }

    fn current(&self) -> Option<usize> {
        self.list.selected().and_then(|i| self.rows.get(i).copied())
    }

    fn sorted_edges(&self, edges: &[usize], by_target: bool) -> Vec<usize> {
        let g = self.g;
        let mut v = edges.to_vec();
        let key = |e: usize| {
            let m = if by_target { g.edges[e].to } else { g.edges[e].from };
            (!g.edges[e].circular, g.modules[m].kind, g.modules[m].id.as_str())
        };
        v.sort_by(|&a, &b| key(a).cmp(&key(b)));
        v
    }

    fn cur_deps(&self) -> Vec<usize> {
        self.current().map(|m| self.sorted_edges(&self.g.out[m], true)).unwrap_or_default()
    }

    fn cur_rdeps(&self) -> Vec<usize> {
        self.current().map(|m| self.sorted_edges(&self.g.inc[m], false)).unwrap_or_default()
    }

    fn reset_side(&mut self) {
        self.deps = select_first(self.cur_deps().len());
        self.rdeps = select_first(self.cur_rdeps().len());
    }

    fn rebuild(&mut self) {
        let g = self.g;
        let keep = self.current();
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(String::from).collect();
        let mut rows: Vec<usize> = (0..g.modules.len())
            .filter(|&m| self.show_ext || g.modules[m].kind == ModuleKind::Local)
            .filter(|&m| {
                let id = g.modules[m].id.to_lowercase();
                terms.iter().all(|t| id.contains(t.as_str()))
            })
            .collect();
        let by_id = |a: &usize, b: &usize| g.modules[*a].id.cmp(&g.modules[*b].id);
        match self.sort {
            Sort::Path => rows.sort_by(|a, b| g.modules[*a].kind.cmp(&g.modules[*b].kind).then_with(|| by_id(a, b))),
            Sort::FanIn => rows.sort_by(|a, b| g.fan_in(*b).cmp(&g.fan_in(*a)).then_with(|| by_id(a, b))),
            Sort::FanOut => rows.sort_by(|a, b| g.fan_out(*b).cmp(&g.fan_out(*a)).then_with(|| by_id(a, b))),
            Sort::Instability => rows.sort_by(|a, b| {
                g.instability(*b).total_cmp(&g.instability(*a)).then_with(|| by_id(a, b))
            }),
        }
        self.rows = rows;
        let sel = keep
            .and_then(|k| self.rows.iter().position(|&r| r == k))
            .or(if self.rows.is_empty() { None } else { Some(0) });
        self.list.select(sel);
        self.reset_side();
    }

    fn goto(&mut self, m: usize) {
        if let Some(c) = self.current()
            && c != m {
                self.history.push(c);
            }
        self.show(m);
    }

    fn show(&mut self, m: usize) {
        if !self.rows.contains(&m) {
            self.filter.clear();
            if self.g.modules[m].kind != ModuleKind::Local {
                self.show_ext = true;
            }
            self.rebuild();
        }
        self.list.select(self.rows.iter().position(|&r| r == m));
        self.reset_side();
        self.tab = Tab::Modules;
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.help {
            self.help = false;
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.editing {
            match k.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.editing = false;
                    self.rebuild();
                }
                KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.editing = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.rebuild();
                }
                _ => {}
            }
            return;
        }
        let idx = TABS.iter().position(|t| *t == self.tab).unwrap();
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => self.reload = true,
            KeyCode::Char(c @ '1'..='4') => self.tab = TABS[c as usize - '1' as usize],
            KeyCode::Tab => self.tab = TABS[(idx + 1) % TABS.len()],
            KeyCode::BackTab => self.tab = TABS[(idx + TABS.len() - 1) % TABS.len()],
            _ => match self.tab {
                Tab::Modules => self.key_modules(k.code),
                Tab::Violations => self.key_violations(k.code),
                Tab::Cycles => self.key_cycles(k.code),
                Tab::Hotspots => self.key_hotspots(k.code),
            },
        }
    }

    fn delta(code: KeyCode) -> Option<isize> {
        Some(match code {
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::PageUp => -PAGE,
            KeyCode::PageDown => PAGE,
            KeyCode::Home | KeyCode::Char('g') => isize::MIN / 2,
            KeyCode::End | KeyCode::Char('G') => isize::MAX / 2,
            _ => return None,
        })
    }

    fn key_modules(&mut self, code: KeyCode) {
        if let Some(d) = Self::delta(code) {
            match self.pane {
                Pane::List => {
                    mv(&mut self.list, self.rows.len(), d);
                    self.reset_side();
                }
                Pane::Deps => {
                    let n = self.cur_deps().len();
                    mv(&mut self.deps, n, d)
                }
                Pane::Dependents => {
                    let n = self.cur_rdeps().len();
                    mv(&mut self.rdeps, n, d)
                }
            }
            return;
        }
        match code {
            KeyCode::Right | KeyCode::Char('l') => {
                self.pane = match self.pane {
                    Pane::List => Pane::Deps,
                    _ => Pane::Dependents,
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.pane = match self.pane {
                    Pane::Dependents => Pane::Deps,
                    _ => Pane::List,
                }
            }
            KeyCode::Enter => match self.pane {
                Pane::List => {
                    self.pane = if !self.cur_deps().is_empty() { Pane::Deps } else { Pane::Dependents }
                }
                Pane::Deps => {
                    if let Some(&e) = self.deps.selected().and_then(|i| self.cur_deps().get(i).copied()).as_ref() {
                        self.goto(self.g.edges[e].to);
                    }
                }
                Pane::Dependents => {
                    if let Some(&e) = self.rdeps.selected().and_then(|i| self.cur_rdeps().get(i).copied()).as_ref() {
                        self.goto(self.g.edges[e].from);
                    }
                }
            },
            KeyCode::Backspace | KeyCode::Char('b') => {
                if let Some(m) = self.history.pop() {
                    self.show(m);
                }
            }
            KeyCode::Char('/') => {
                self.editing = true;
                self.pane = Pane::List;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Esc => self.pane = Pane::List,
            KeyCode::Char('e') => {
                self.show_ext = !self.show_ext;
                self.rebuild();
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild();
            }
            KeyCode::Char('c') => {
                if let Some(c) = self.current().and_then(|m| self.g.cycle_of[m]) {
                    self.clist.select(Some(c));
                    self.tab = Tab::Cycles;
                }
            }
            KeyCode::Char('v') => {
                if let Some(&v) = self.current().and_then(|m| self.viol_of[m].first()) {
                    self.vlist.select(Some(v));
                    self.tab = Tab::Violations;
                }
            }
            _ => {}
        }
    }

    fn key_violations(&mut self, code: KeyCode) {
        let vs = &self.a.violations;
        if let Some(d) = Self::delta(code) {
            mv(&mut self.vlist, vs.len(), d);
            return;
        }
        let Some(v) = self.vlist.selected().and_then(|i| vs.get(i)) else { return };
        let target = match code {
            KeyCode::Enter => Some(v.from),
            KeyCode::Char('t') => v.to,
            _ => None,
        };
        let Some(m) = target else { return };
        match v.scope {
            Scope::Module => self.goto(m),
            Scope::Folder => {
                // Show the folder's modules.
                let folder = &v.graph(self.g).modules[m].id;
                self.filter = if folder == "." { String::new() } else { format!("{folder}/") };
                self.tab = Tab::Modules;
                self.pane = Pane::List;
                self.rebuild();
            }
        }
    }

    fn key_cycles(&mut self, code: KeyCode) {
        if let Some(d) = Self::delta(code) {
            mv(&mut self.clist, self.g.cycles.len(), d);
            return;
        }
        if code == KeyCode::Enter
            && let Some(c) = self.clist.selected() {
                self.goto(self.cycle_paths[c][0]);
            }
    }

    fn key_hotspots(&mut self, code: KeyCode) {
        let col = self.hot_col;
        if let Some(d) = Self::delta(code) {
            mv(&mut self.hot[col], self.hot_rows[col].len(), d);
            return;
        }
        match code {
            KeyCode::Right | KeyCode::Char('l') => self.hot_col = (col + 1).min(2),
            KeyCode::Left | KeyCode::Char('h') => self.hot_col = col.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(&m) = self.hot[col].selected().and_then(|i| self.hot_rows[col].get(i)) {
                    self.goto(m);
                }
            }
            _ => {}
        }
    }

    // ───────────────────────────── drawing ─────────────────────────────

    fn draw(&mut self, f: &mut Frame) {
        let [top, body, bottom] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        self.draw_tabs(f, top);
        match self.tab {
            Tab::Modules => self.draw_modules(f, body),
            Tab::Violations => self.draw_violations(f, body),
            Tab::Cycles => self.draw_cycles(f, body),
            Tab::Hotspots => self.draw_hotspots(f, body),
        }
        self.draw_keys(f, bottom);
        if self.help {
            self.draw_help(f);
        }
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let g = self.g;
        let (e, w, _) = crate::report::counts(&self.a.violations);
        let titles = [
            "Modules".to_string(),
            format!("Violations {}", self.a.violations.len()),
            format!("Cycles {}", g.cycles.len()),
            "Hotspots".to_string(),
        ];
        let mut spans = vec![Span::styled(" ⧉ tangle ", Style::new().bg(ACCENT).fg(Color::Black).bold()), Span::raw(" ")];
        for (i, t) in titles.iter().enumerate() {
            let style = if TABS[i] == self.tab {
                Style::new().fg(ACCENT).bold().underlined()
            } else {
                Style::new().fg(Color::Gray)
            };
            spans.push(Span::styled(format!(" {} {t} ", i + 1), style));
        }
        f.render_widget(Line::from(spans), area);
        let mut right = vec![Span::styled(
            format!("{} modules · {} deps · {:.0}ms ", g.local_count(), g.edges.len(), g.total_ms()),
            Style::new().dim(),
        )];
        if e > 0 {
            right.insert(0, Span::styled(format!("✖ {} · ", crate::report::plural(e, "error")), Style::new().fg(Color::Red).bold()));
        } else if w > 0 {
            right.insert(0, Span::styled(format!("⚠ {} · ", crate::report::plural(w, "warning")), Style::new().fg(Color::Yellow).bold()));
        } else {
            right.insert(0, Span::styled("✔ clean · ", Style::new().fg(Color::Green).bold()));
        }
        f.render_widget(Line::from(right).right_aligned(), area);
    }

    fn module_marker(&self, m: usize) -> Span<'static> {
        if self.g.cycle_of[m].is_some() {
            Span::styled("⟳ ", Style::new().fg(Color::Red).bold())
        } else if let Some(s) = self.worst[m] {
            Span::styled("● ", sev_style(s))
        } else {
            Span::raw("  ")
        }
    }

    fn draw_modules(&mut self, f: &mut Frame, area: Rect) {
        let g = self.g;
        let [main, detail] = Layout::vertical([Constraint::Min(0), Constraint::Length(7)]).areas(area);
        let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main);
        let [rt, rb] = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(right);

        // Module list.
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|&m| {
                let mut spans = vec![
                    self.module_marker(m),
                    Span::styled(format!("{:>4} {:>4}  ", g.fan_in(m), g.fan_out(m)), Style::new().dim()),
                ];
                spans.extend(path_spans(&g.modules[m].id, g.modules[m].kind));
                ListItem::new(Line::from(spans))
            })
            .collect();
        let mut title = vec![
            Span::styled(" Modules ", Style::new().bold()),
            Span::styled(format!("{}  ", self.rows.len()), Style::new().dim()),
            Span::styled(format!("in  out · sort: {} ", self.sort.label()), Style::new().dim()),
        ];
        if self.editing || !self.filter.is_empty() {
            title.push(Span::styled(
                format!(" /{}{} ", self.filter, if self.editing { "▏" } else { "" }),
                Style::new().fg(Color::Black).bg(Color::Yellow),
            ));
        }
        let focused = self.pane == Pane::List;
        f.render_stateful_widget(list_widget(items, block(Line::from(title), focused), focused), left, &mut self.list);

        // Dependencies / dependents.
        let deps = self.cur_deps();
        let rdeps = self.cur_rdeps();
        let edge_item = |e: usize, target: bool| {
            let edge = &g.edges[e];
            let m = if target { edge.to } else { edge.from };
            let mut spans = vec![if edge.circular {
                Span::styled("⟳ ", Style::new().fg(Color::Red).bold())
            } else {
                Span::raw("  ")
            }];
            spans.extend(path_spans(&g.modules[m].id, g.modules[m].kind));
            spans.extend(edge_tags(edge));
            ListItem::new(Line::from(spans))
        };
        let di: Vec<ListItem> = deps.iter().map(|&e| edge_item(e, true)).collect();
        let ri: Vec<ListItem> = rdeps.iter().map(|&e| edge_item(e, false)).collect();
        let fd = self.pane == Pane::Deps;
        let fr = self.pane == Pane::Dependents;
        let t = |name: &str, n: usize, arrow: &str| {
            Line::from(vec![
                Span::styled(format!(" {arrow} {name} "), Style::new().bold()),
                Span::styled(format!("{n} "), Style::new().dim()),
            ])
        };
        f.render_stateful_widget(list_widget(di, block(t("Imports", deps.len(), "→"), fd), fd), rt, &mut self.deps);
        f.render_stateful_widget(list_widget(ri, block(t("Imported by", rdeps.len(), "←"), fr), fr), rb, &mut self.rdeps);

        // Details.
        let mut lines = vec![];
        if let Some(m) = self.current() {
            let module = &g.modules[m];
            let mut head = vec![Span::styled(module.id.clone(), Style::new().bold().fg(kind_color(module.kind)))];
            head.push(Span::styled(format!("  {}", module.kind.as_str()), Style::new().dim()));
            if module.parse_errors > 0 {
                head.push(Span::styled(format!("  {} parse errors", module.parse_errors), Style::new().fg(Color::Yellow)));
            }
            lines.push(Line::from(head));
            let mut stats = vec![
                Span::raw(format!("fan-in {} · fan-out {} · instability {:.2}", g.fan_in(m), g.fan_out(m), g.instability(m))),
            ];
            if let Some(c) = g.cycle_of[m] {
                stats.push(Span::styled(
                    format!("  ⟳ in cycle #{} ({} modules) — press c", c + 1, g.cycles[c].len()),
                    Style::new().fg(Color::Red),
                ));
            }
            if g.is_orphan(m) {
                stats.push(Span::styled("  orphan", Style::new().fg(Color::Yellow)));
            }
            lines.push(Line::from(stats));
            for &vi in self.viol_of[m].iter().take(3) {
                let v = &self.a.violations[vi];
                let mut l = vec![
                    Span::styled(format!("{:<5} ", v.severity.as_str()), sev_style(v.severity)),
                    Span::styled(v.rule.clone(), Style::new().bold()),
                    Span::raw("  "),
                    Span::styled(v.source_id(g).to_string(), Style::new().dim()),
                ];
                if let Some(t) = v.target_id(g) {
                    l.push(Span::styled(format!(" → {t}"), Style::new().dim()));
                }
                lines.push(Line::from(l));
            }
            let more = self.viol_of[m].len().saturating_sub(3);
            if more > 0 {
                lines.push(Line::styled(format!("+{more} more — press v"), Style::new().dim()));
            }
        } else {
            lines.push(Line::styled("no modules match", Style::new().dim()));
        }
        f.render_widget(Paragraph::new(lines).block(block(" Details ", false)), detail);
    }

    fn draw_violations(&mut self, f: &mut Frame, area: Rect) {
        let g = self.g;
        let vs = &self.a.violations;
        if vs.is_empty() {
            let p = Paragraph::new(Line::styled("✔ No rule violations.", Style::new().fg(Color::Green).bold()))
                .alignment(Alignment::Center)
                .block(block(" Violations ", true));
            f.render_widget(p, area);
            return;
        }
        let [top, bottom] = Layout::vertical([Constraint::Min(0), Constraint::Length(10)]).areas(area);
        let rule_w = vs.iter().map(|v| v.rule.len()).max().unwrap_or(0);
        let items: Vec<ListItem> = vs
            .iter()
            .map(|v| {
                let mut spans = vec![
                    Span::styled(format!("{:<5} ", v.severity.as_str()), sev_style(v.severity)),
                    Span::styled(format!("{:<rule_w$}  ", v.rule), Style::new().bold()),
                ];
                let vg = v.graph(g);
                spans.extend(path_spans(&vg.modules[v.from].id, vg.modules[v.from].kind));
                if let Some(t) = v.to {
                    spans.push(Span::styled(" → ", Style::new().dim()));
                    spans.extend(path_spans(&vg.modules[t].id, vg.modules[t].kind));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let title = Line::from(vec![Span::styled(" Violations ", Style::new().bold()), Span::styled(format!("{} ", vs.len()), Style::new().dim())]);
        f.render_stateful_widget(list_widget(items, block(title, true), true), top, &mut self.vlist);

        let mut lines = vec![];
        if let Some(v) = self.vlist.selected().and_then(|i| vs.get(i)) {
            lines.push(Line::from(vec![
                Span::styled(v.rule.clone(), Style::new().bold()),
                Span::raw("  "),
                Span::styled(v.severity.as_str(), sev_style(v.severity)),
            ]));
            if let Some(c) = &v.comment {
                lines.push(Line::styled(c.clone(), Style::new().italic().fg(Color::Gray)));
            }
            let vg = v.graph(g);
            let scope = if v.scope == Scope::Folder { "  (folder scope — ⏎ lists the folder's modules)" } else { "" };
            lines.push(Line::from(vec![
                Span::styled("from  ", Style::new().dim()),
                Span::raw(v.source_id(g).to_string()),
                Span::styled(scope, Style::new().dim()),
            ]));
            if let Some(t) = v.to {
                let edge = vg.out[v.from].iter().map(|&e| &vg.edges[e]).find(|e| e.to == t);
                let mut l = vec![Span::styled("to    ", Style::new().dim())];
                l.extend(path_spans(&vg.modules[t].id, vg.modules[t].kind));
                if let Some(e) = edge.filter(|_| v.scope == Scope::Module) {
                    l.push(Span::styled(format!("   via {:?}", e.specifier), Style::new().dim()));
                    l.extend(edge_tags(e));
                }
                lines.push(Line::from(l));
            }
            if v.cycle.len() > 1 {
                let chain = v.cycle_ids(g).join(" → ");
                lines.push(Line::from(vec![
                    Span::styled("cycle ", Style::new().dim()),
                    Span::styled(chain, Style::new().fg(Color::Red)),
                ]));
            }
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block(" Details ", false)), bottom);
    }

    fn draw_cycles(&mut self, f: &mut Frame, area: Rect) {
        let g = self.g;
        if g.cycles.is_empty() {
            let p = Paragraph::new(Line::styled("✔ No circular dependencies.", Style::new().fg(Color::Green).bold()))
                .alignment(Alignment::Center)
                .block(block(" Cycles ", true));
            f.render_widget(p, area);
            return;
        }
        let [left, right] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);
        let items: Vec<ListItem> = self
            .cycle_paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let mut spans = vec![
                    Span::styled(format!("#{:<3}", i + 1), Style::new().fg(Color::Red).bold()),
                    Span::styled(format!("{:>3} modules  ", g.cycles[i].len()), Style::new().dim()),
                ];
                spans.extend(path_spans(&g.modules[path[0]].id, ModuleKind::Local));
                ListItem::new(Line::from(spans))
            })
            .collect();
        let title = Line::from(vec![Span::styled(" Cycles ", Style::new().bold()), Span::styled(format!("{} ", g.cycles.len()), Style::new().dim())]);
        f.render_stateful_widget(list_widget(items, block(title, true), true), left, &mut self.clist);

        let mut lines = vec![];
        if let Some(c) = self.clist.selected() {
            lines.push(Line::styled("Shortest loop", Style::new().bold()));
            let path = &self.cycle_paths[c];
            for (i, &m) in path.iter().enumerate() {
                if i > 0 {
                    let edge = g.out[path[i - 1]].iter().map(|&e| &g.edges[e]).find(|e| e.to == m);
                    let mut l = vec![Span::styled("  ↓ ", Style::new().fg(Color::Red))];
                    if let Some(e) = edge {
                        l.push(Span::styled(format!("{:?}", e.specifier), Style::new().dim()));
                        l.extend(edge_tags(e));
                    }
                    lines.push(Line::from(l));
                }
                lines.push(Line::from(path_spans(&g.modules[m].id, ModuleKind::Local)));
            }
            if g.cycles[c].len() + 1 > path.len() {
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("All {} modules in this tangle", g.cycles[c].len()), Style::new().bold()));
                for &m in &g.cycles[c] {
                    let mut l = vec![Span::raw("  ")];
                    l.extend(path_spans(&g.modules[m].id, ModuleKind::Local));
                    lines.push(Line::from(l));
                }
            }
        }
        f.render_widget(Paragraph::new(lines).block(block(" Cycle ", false)), right);
    }

    fn draw_hotspots(&mut self, f: &mut Frame, area: Rect) {
        let g = self.g;
        let [summary, lists] = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).areas(area);
        let count = |k| g.modules.iter().filter(|m| m.kind == k).count();
        let orphans = (0..g.modules.len()).filter(|&m| g.is_orphan(m)).count();
        let t = g.timings;
        let stat = |label: &str, value: String, color: Color| {
            vec![Span::styled(format!("{label} "), Style::new().dim()), Span::styled(value, Style::new().fg(color).bold()), Span::raw("   ")]
        };
        let l1: Vec<Span> = [
            stat("modules", g.local_count().to_string(), Color::Reset),
            stat("dependencies", g.edges.len().to_string(), Color::Reset),
            stat("packages", count(ModuleKind::Npm).to_string(), Color::Cyan),
            stat("builtins", count(ModuleKind::Builtin).to_string(), Color::Green),
        ]
        .concat();
        let l2: Vec<Span> = [
            stat("cycles", g.cycles.len().to_string(), if g.cycles.is_empty() { Color::Green } else { Color::Red }),
            stat("unresolved", count(ModuleKind::Unresolved).to_string(), if count(ModuleKind::Unresolved) == 0 { Color::Green } else { Color::Red }),
            stat("orphans", orphans.to_string(), Color::Yellow),
        ]
        .concat();
        let l3 = Line::styled(
            format!("walk {:.1}ms · parse+resolve {:.1}ms · graph {:.1}ms", t.walk_ms, t.parse_ms, t.graph_ms),
            Style::new().dim(),
        );
        f.render_widget(Paragraph::new(vec![Line::from(l1), Line::from(l2), l3]).block(block(" Overview ", false)), summary);

        let cols = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(lists);
        let titles = ["Most depended-on", "Most dependencies", "Most used packages"];
        for c in 0..3 {
            let items: Vec<ListItem> = self.hot_rows[c]
                .iter()
                .map(|&m| {
                    let n = if c == 1 { g.fan_out(m) } else { g.fan_in(m) };
                    let mut spans = vec![Span::styled(format!("{n:>5}  "), Style::new().bold())];
                    spans.extend(path_spans(&g.modules[m].id, g.modules[m].kind));
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let focused = self.hot_col == c;
            f.render_stateful_widget(list_widget(items, block(format!(" {} ", titles[c]), focused), focused), cols[c], &mut self.hot[c]);
        }
    }

    fn draw_keys(&self, f: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = if self.editing {
            &[("type", "filter"), ("⏎", "done"), ("esc", "clear")]
        } else {
            match self.tab {
                Tab::Modules => &[
                    ("↑↓", "move"), ("←→", "pane"), ("⏎", "open"), ("⌫", "back"), ("/", "filter"),
                    ("s", "sort"), ("e", "externals"), ("⇥", "tab"), ("?", "help"), ("q", "quit"),
                ],
                Tab::Violations => &[("↑↓", "move"), ("⏎", "go to source"), ("t", "go to target"), ("⇥", "tab"), ("q", "quit")],
                Tab::Cycles => &[("↑↓", "move"), ("⏎", "open module"), ("⇥", "tab"), ("q", "quit")],
                Tab::Hotspots => &[("↑↓", "move"), ("←→", "column"), ("⏎", "open module"), ("⇥", "tab"), ("q", "quit")],
            }
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, d) in keys {
            spans.push(Span::styled(format!(" {k} "), Style::new().bg(Color::DarkGray).fg(Color::White)));
            spans.push(Span::styled(format!(" {d}  "), Style::new().dim()));
        }
        if !self.history.is_empty() && self.tab == Tab::Modules {
            spans.push(Span::styled(format!("history {}", self.history.len()), Style::new().dim()));
        }
        let mut right = vec![];
        if let Some(st) = &self.status {
            let color = if st.starts_with('✖') { Color::Red } else { Color::Gray };
            right.push(Span::styled(format!("{st}  "), Style::new().fg(color)));
        }
        if self.live {
            right.push(Span::styled("● live ", Style::new().fg(Color::Green).bold()));
        }
        let right = Line::from(right);
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right.width() as u16)]).areas(area);
        f.render_widget(Line::from(spans), left_area);
        f.render_widget(right, right_area);
    }

    fn draw_help(&self, f: &mut Frame) {
        let area = f.area();
        let w = 64.min(area.width.saturating_sub(4));
        let h = 24.min(area.height.saturating_sub(2));
        let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
        let row = |k: &str, d: &str| {
            Line::from(vec![Span::styled(format!("  {k:<14}"), Style::new().fg(ACCENT).bold()), Span::raw(d.to_string())])
        };
        let lines = vec![
            Line::styled(" Navigation", Style::new().bold()),
            row("1-4 / ⇥", "switch tab"),
            row("↑↓ j k", "move · PgUp/PgDn page · g/G ends"),
            row("←→ h l", "switch pane (modules / imports / imported by)"),
            row("⏎", "follow the selected dependency"),
            row("⌫ b", "go back"),
            row("r", "rebuild now (auto-rebuilds on file changes)"),
            Line::raw(""),
            Line::styled(" Modules", Style::new().bold()),
            row("/", "filter (space-separated terms, all must match)"),
            row("s", "sort: path → fan-in → fan-out → instability"),
            row("e", "show npm packages / builtins / unresolved"),
            row("c / v", "jump to this module's cycle / violations"),
            Line::raw(""),
            Line::styled(" Legend", Style::new().bold()),
            Line::from(vec![Span::styled("  ⟳ ", Style::new().fg(Color::Red)), Span::raw("part of a cycle   "), Span::styled("● ", Style::new().fg(Color::Red)), Span::raw("has violations")]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("npm ", Style::new().fg(Color::Cyan)),
                Span::styled("core ", Style::new().fg(Color::Green)),
                Span::styled("unresolved ", Style::new().fg(Color::Red)),
                Span::styled("[type] ", Style::new().fg(Color::Blue)),
                Span::styled("[dynamic] ", Style::new().fg(Color::Yellow)),
                Span::styled("[dev]", Style::new().fg(Color::Magenta)),
            ]),
            Line::raw(""),
            Line::styled("  any key to close", Style::new().dim()),
        ];
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(lines).block(block(" Help ", true)), r);
    }
}
