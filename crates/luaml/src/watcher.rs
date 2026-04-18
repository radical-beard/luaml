//! File-system watcher for hot-reloading `.luaml` scripts.
//!
//! Requires the `file-watch` feature. Engine-internal — consumers do not
//! interact with `ScriptWatcher` directly. They call [`crate::LuamlEngine::watch`]
//! and the engine drains pending changes on every dispatch.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};

use crate::error::LuamlError;
use crate::registry::ScriptRegistry;

/// A file change event produced by the watcher.
#[derive(Debug, Clone)]
pub(crate) enum FileChange {
    /// File was created or modified — re-read and re-register.
    CreateOrModify(PathBuf),
    /// File was removed — unregister.
    Remove(PathBuf),
}

/// Watches directories for `.luaml` file changes and queues updates. The
/// engine drains the queue on every dispatch; callers never touch this type.
pub(crate) struct ScriptWatcher {
    rx: mpsc::Receiver<Vec<FileChange>>,
    // Hold the debouncer to keep the watcher thread alive.
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
}

impl ScriptWatcher {
    /// Start watching the given directories for `.luaml` file changes.
    pub(crate) fn new(dirs: &[&Path], debounce: std::time::Duration) -> Result<Self, LuamlError> {
        let (tx, rx) = mpsc::channel();

        let mut debouncer = new_debouncer(
            debounce,
            move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                let events = match res {
                    Ok(events) => events,
                    Err(_) => return,
                };

                let mut changes = Vec::new();
                for event in events {
                    let path = event.path;
                    if path.extension().is_some_and(|ext| ext == "luaml") {
                        if let DebouncedEventKind::Any = event.kind {
                            if path.exists() {
                                changes.push(FileChange::CreateOrModify(path));
                            } else {
                                changes.push(FileChange::Remove(path));
                            }
                        }
                    }
                }

                if !changes.is_empty() {
                    let _ = tx.send(changes);
                }
            },
        )
        .map_err(|e| LuamlError::Io(std::io::Error::other(e.to_string())))?;

        for dir in dirs {
            debouncer
                .watcher()
                .watch(dir, RecursiveMode::Recursive)
                .map_err(|e| LuamlError::Io(std::io::Error::other(e.to_string())))?;
        }

        Ok(Self {
            rx,
            _debouncer: debouncer,
        })
    }

    /// Drain pending changes and apply each one to the registry. Parse errors
    /// from a single changed file are logged to stderr but do not stop other
    /// changes from applying. Returns the paths that were touched.
    pub(crate) fn apply_pending(&self, registry: &mut ScriptRegistry) -> Vec<PathBuf> {
        let mut changed = Vec::new();

        while let Ok(batch) = self.rx.try_recv() {
            for change in batch {
                match change {
                    FileChange::CreateOrModify(path) => match std::fs::read_to_string(&path) {
                        Ok(text) => match registry.replace(&path, &text) {
                            Ok(_) => changed.push(path),
                            Err(e) => {
                                eprintln!("[luaml] failed to reload '{}': {e}", path.display())
                            }
                        },
                        Err(e) => eprintln!("[luaml] failed to read '{}': {e}", path.display()),
                    },
                    FileChange::Remove(path) => {
                        let _ = registry.unregister(&path);
                        changed.push(path);
                    }
                }
            }
        }

        changed
    }
}
