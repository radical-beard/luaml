//! File-system watcher for hot-reloading `.luaml` scripts.
//!
//! Requires the `file-watch` feature.
//!
//! The watcher runs on a background thread (notify requirement). Because the
//! Lua VM is `!Send`, all mutations happen on the caller's thread: the watcher
//! sends change events to a channel, and the consumer calls
//! [`ScriptWatcher::process_pending`] to apply them before dispatch.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};

use crate::error::LuamlError;
use crate::registry::ScriptRegistry;

/// A file change event produced by the watcher.
#[derive(Debug, Clone)]
enum FileChange {
    /// File was created or modified — re-read and re-register.
    CreateOrModify(PathBuf),
    /// File was removed — unregister.
    Remove(PathBuf),
}

/// Watches directories for `.luaml` file changes and queues updates for the registry.
pub struct ScriptWatcher {
    rx: mpsc::Receiver<Vec<FileChange>>,
    // Hold the debouncer to keep the watcher thread alive.
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
}

impl ScriptWatcher {
    /// Start watching the given directories for `.luaml` file changes.
    ///
    /// The debounce duration controls how long to wait after the last filesystem
    /// event before sending a batch of changes.
    pub fn new(dirs: &[&Path], debounce: std::time::Duration) -> Result<Self, LuamlError> {
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
                        // DebouncedEventKind::Any covers all change types.
                        // Check filesystem state to determine create/modify vs remove.
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

    /// Apply all pending file changes to the registry.
    ///
    /// Call this before dispatch to ensure the registry reflects the latest
    /// file system state. Returns the paths that were changed (for consumers
    /// that want to invalidate caches).
    pub fn process_pending(
        &self,
        registry: &mut ScriptRegistry,
    ) -> Result<Vec<PathBuf>, LuamlError> {
        let mut changed = Vec::new();

        while let Ok(batch) = self.rx.try_recv() {
            for change in batch {
                match change {
                    FileChange::CreateOrModify(path) => {
                        let text = std::fs::read_to_string(&path)?;
                        registry.replace(&path, &text)?;
                        changed.push(path);
                    }
                    FileChange::Remove(path) => {
                        registry.unregister(&path);
                        changed.push(path);
                    }
                }
            }
        }

        Ok(changed)
    }
}
