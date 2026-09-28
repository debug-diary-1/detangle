//! Debounced filesystem watching for `detangle watch` and the live explorer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::scan::has_source_ext;

/// How long the filesystem must be quiet before a batch of changes is released
/// (editors and formatters often write several times per save).
const QUIET: Duration = Duration::from_millis(150);

/// A settled batch of filesystem changes.
#[derive(Debug, Default)]
pub struct Changes {
    pub paths: BTreeSet<PathBuf>,
    /// tsconfig / package.json / detangle.toml changed: everything must be rebuilt.
    pub config: bool,
    /// The OS dropped events (e.g. an inotify queue overflow, or FSEvents
    /// asking for a subdirectory rescan), so `paths` may be incomplete:
    /// everything must be rebuilt.
    pub rescan: bool,
}

impl Changes {
    /// Forces a full rebuild.
    pub fn full() -> Self {
        Changes { config: true, ..Default::default() }
    }

    /// "src/a.ts" or "src/a.ts +3 more".
    pub fn describe(&self, root: &Path) -> String {
        let rel = |p: &PathBuf| p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/");
        match (self.paths.iter().next(), self.paths.len()) {
            (None, _) => String::new(),
            (Some(p), 1) => rel(p),
            (Some(p), n) => format!("{} +{} more", rel(p), n - 1),
        }
    }

    /// Whether the project must be re-opened rather than updated.
    pub fn needs_full(&self) -> bool {
        self.config || self.rescan
    }

    fn add(&mut self, msg: Msg) {
        match msg {
            Msg::Path(path, config) => {
                self.paths.insert(path);
                self.config |= config;
            }
            Msg::Rescan => self.rescan = true,
        }
    }
}

/// What the watcher callback passes on from one `notify` event.
#[derive(Debug, PartialEq)]
enum Msg {
    /// A path that matters, and whether it's a config file.
    Path(PathBuf, bool),
    /// Events were lost.
    Rescan,
}

pub struct Watcher {
    _inner: RecommendedWatcher,
    rx: Receiver<Msg>,
    pending: Changes,
    last_event: Option<Instant>,
}

fn is_config(name: &str) -> bool {
    name == "detangle.toml"
        || name == "package.json"
        || (name.starts_with("tsconfig") && name.ends_with(".json"))
        || name.starts_with("webpack.config.")
        || name.starts_with("vite.config.")
        || name.starts_with("babel.config.")
        || name.starts_with(".babelrc")
        || name.starts_with(".env")
        || name == "project.json"
}

/// The messages one `notify` event produces: the paths that matter, or
/// `Rescan` when events were lost.
fn messages(ev: notify::Event) -> Vec<Msg> {
    // Checked before the kind filter: lost-event notices come as `Other`.
    if ev.need_rescan() {
        return vec![Msg::Rescan];
    }
    let structural = match ev.kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => return Vec::new(),
        EventKind::Modify(_) => false,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for p in ev.paths {
        let ignored = p
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("node_modules" | ".git" | "target")));
        if ignored {
            continue;
        }
        let config = p.file_name().and_then(|n| n.to_str()).is_some_and(is_config);
        // Extension-less paths matter only when created/removed/renamed:
        // they may be directories full of sources.
        if config || has_source_ext(&p) || (structural && p.extension().is_none()) {
            out.push(Msg::Path(p, config));
        }
    }
    out
}

impl Watcher {
    pub fn new(root: &Path) -> Result<Self> {
        let (tx, rx) = channel();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            for msg in messages(ev) {
                let _ = tx.send(msg);
            }
        })?;
        inner.watch(root, RecursiveMode::Recursive)?;
        Ok(Self { _inner: inner, rx, pending: Changes::default(), last_event: None })
    }

    fn add(&mut self, msg: Msg) {
        self.pending.add(msg);
        self.last_event = Some(Instant::now());
    }

    fn take(&mut self) -> Changes {
        self.last_event = None;
        std::mem::take(&mut self.pending)
    }

    /// Non-blocking: returns the changes once a burst has settled.
    pub fn poll(&mut self) -> Option<Changes> {
        while let Ok(ev) = self.rx.try_recv() {
            self.add(ev);
        }
        match self.last_event {
            Some(t) if t.elapsed() >= QUIET => Some(self.take()),
            _ => None,
        }
    }

    /// Blocks until a burst of changes has settled.
    pub fn wait(&mut self) -> Changes {
        let Ok(first) = self.rx.recv() else { return Changes::default() };
        self.add(first);
        loop {
            match self.rx.recv_timeout(QUIET) {
                Ok(ev) => self.add(ev),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return self.take(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, Flag};

    fn changes(ev: notify::Event) -> Changes {
        let mut c = Changes::default();
        for m in messages(ev) {
            c.add(m);
        }
        c
    }

    /// Lost events come as `Other` with the rescan flag; the kind filter
    /// used to drop them.
    #[test]
    fn lost_events_force_a_full_rebuild() {
        let c = changes(notify::Event::new(EventKind::Other).set_flag(Flag::Rescan));
        assert!(c.rescan && c.needs_full());
        assert!(!c.config && c.paths.is_empty());

        let c = changes(notify::Event::new(EventKind::Other));
        assert!(!c.needs_full());

        let c = changes(notify::Event::new(EventKind::Create(CreateKind::File)).add_path("/p/src/a.ts".into()));
        assert!(!c.needs_full());
        assert_eq!(c.paths.len(), 1);
    }
}
