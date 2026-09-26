//! Debounced filesystem watching for `tangle watch` and the live explorer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::scan::SOURCE_EXTS;

/// How long the filesystem must be quiet before a batch of changes is released
/// (editors and formatters often write several times per save).
const QUIET: Duration = Duration::from_millis(150);

pub struct Watcher {
    _inner: RecommendedWatcher,
    rx: Receiver<PathBuf>,
    root: PathBuf,
    pending: BTreeSet<PathBuf>,
    last_event: Option<Instant>,
}

/// Files whose changes can alter the dependency graph or the rules.
fn relevant(p: &Path) -> bool {
    if p.components().any(|c| matches!(c.as_os_str().to_str(), Some("node_modules" | ".git" | "target"))) {
        return false;
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    SOURCE_EXTS.contains(&ext)
        || name == "tangle.toml"
        || name == "package.json"
        || (name.starts_with("tsconfig") && ext == "json")
}

impl Watcher {
    pub fn new(root: &Path) -> Result<Self> {
        let (tx, rx) = channel();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            let content_change = match ev.kind {
                EventKind::Create(_) | EventKind::Remove(_) => true,
                EventKind::Modify(ModifyKind::Metadata(_)) => false,
                EventKind::Modify(_) => true,
                _ => false,
            };
            if content_change {
                for p in ev.paths.into_iter().filter(|p| relevant(p)) {
                    let _ = tx.send(p);
                }
            }
        })?;
        inner.watch(root, RecursiveMode::Recursive)?;
        Ok(Self { _inner: inner, rx, root: root.to_path_buf(), pending: BTreeSet::new(), last_event: None })
    }

    fn take(&mut self) -> Vec<String> {
        self.last_event = None;
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|p| p.strip_prefix(&self.root).unwrap_or(&p).to_string_lossy().replace('\\', "/"))
            .collect()
    }

    /// Non-blocking. Returns the changed paths (root-relative) once a burst of
    /// changes has settled, otherwise `None`.
    pub fn poll(&mut self) -> Option<Vec<String>> {
        while let Ok(p) = self.rx.try_recv() {
            self.pending.insert(p);
            self.last_event = Some(Instant::now());
        }
        match self.last_event {
            Some(t) if t.elapsed() >= QUIET => Some(self.take()),
            _ => None,
        }
    }

    /// Blocks until a burst of changes has settled; returns the changed paths.
    pub fn wait(&mut self) -> Vec<String> {
        let Ok(first) = self.rx.recv() else { return vec![] };
        self.pending.insert(first);
        loop {
            match self.rx.recv_timeout(QUIET) {
                Ok(p) => {
                    self.pending.insert(p);
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return self.take(),
            }
        }
    }
}

/// "src/a.ts" or "src/a.ts +3 more".
pub fn describe(changed: &[String]) -> String {
    match changed {
        [] => "changes".into(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} +{} more", rest.len()),
    }
}
