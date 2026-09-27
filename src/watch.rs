//! Debounced filesystem watching for `tangle watch` and the live explorer.

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
    /// tsconfig / package.json / tangle.toml changed: everything must be rebuilt.
    pub config: bool,
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
}

pub struct Watcher {
    _inner: RecommendedWatcher,
    rx: Receiver<(PathBuf, bool)>,
    pending: Changes,
    last_event: Option<Instant>,
}

fn is_config(name: &str) -> bool {
    name == "tangle.toml"
        || name == "package.json"
        || (name.starts_with("tsconfig") && name.ends_with(".json"))
        || name.starts_with("webpack.config.")
        || name.starts_with("vite.config.")
        || name.starts_with("babel.config.")
        || name.starts_with(".babelrc")
        || name.starts_with(".env")
}

impl Watcher {
    pub fn new(root: &Path) -> Result<Self> {
        let (tx, rx) = channel();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            let structural = match ev.kind {
                EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => true,
                EventKind::Modify(ModifyKind::Metadata(_)) => return,
                EventKind::Modify(_) => false,
                _ => return,
            };
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
                    let _ = tx.send((p, config));
                }
            }
        })?;
        inner.watch(root, RecursiveMode::Recursive)?;
        Ok(Self { _inner: inner, rx, pending: Changes::default(), last_event: None })
    }

    fn add(&mut self, (path, config): (PathBuf, bool)) {
        self.pending.paths.insert(path);
        self.pending.config |= config;
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
