//! Server-side poll watcher for AGP-generated ViewBinding classes under `build/`.
//!
//! Editors' native file watchers typically skip gitignored paths, so generated
//! `*Binding.java` files are re-discovered here after Gradle builds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use walkdir::WalkDir;

use crate::indexer::{
    is_generated_binding_watcher_path, DatabindingWatcherHandle, DatabindingWatcherState, Indexer,
};
use crate::workspace::Event;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Spawn a background task that polls registered module roots for generated binding changes.
pub(crate) fn spawn_databinding_watcher(
    indexer: Arc<Indexer>,
    republish_tx: mpsc::Sender<Event>,
) -> DatabindingWatcherHandle {
    spawn_databinding_watcher_with_interval(indexer, republish_tx, DEFAULT_POLL_INTERVAL)
}

pub(crate) fn spawn_databinding_watcher_with_interval(
    indexer: Arc<Indexer>,
    republish_tx: mpsc::Sender<Event>,
    poll_interval: Duration,
) -> DatabindingWatcherHandle {
    let state = Arc::new(DatabindingWatcherState::new());
    let handle = DatabindingWatcherHandle::new(Arc::clone(&state));

    let poll_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut snapshots: HashMap<PathBuf, HashMap<String, u64>> = HashMap::new();
        let mut interval = tokio::time::interval(poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let module_roots = poll_state.registered_module_roots();
            for module_root in module_roots {
                let current_snapshot = snapshot_databinding_binding_mtimes(&module_root);
                let changed = match snapshots.get(&module_root) {
                    Some(previous) => previous != &current_snapshot,
                    None => {
                        snapshots.insert(module_root.clone(), current_snapshot.clone());
                        module_needs_binding_index(&indexer, &module_root, &current_snapshot)
                    }
                };
                if !changed {
                    continue;
                }
                snapshots.insert(module_root.clone(), current_snapshot);
                let indexer = Arc::clone(&indexer);
                let module = module_root.clone();
                let republish_tx = republish_tx.clone();
                tokio::task::spawn_blocking(move || {
                    indexer.index_generated_bindings(&module);
                })
                .await
                .ok();
                let _ = republish_tx.try_send(Event::RepublishOpenFileDiagnostics);
            }
        }
    });

    handle
}

/// True when bindings exist on disk but the side index has no entry for `module_root`.
fn module_needs_binding_index(
    indexer: &Indexer,
    module_root: &Path,
    current_snapshot: &HashMap<String, u64>,
) -> bool {
    if current_snapshot.is_empty() {
        return false;
    }
    match indexer.generated_bindings.get(module_root) {
        Some(module_bindings) => module_bindings.entries.is_empty(),
        None => true,
    }
}

/// Snapshot class name → mtime for `*Binding.java` files under a `databinding` path segment.
fn snapshot_databinding_binding_mtimes(module_root: &Path) -> HashMap<String, u64> {
    let build_dir = module_root.join("build");
    if !build_dir.is_dir() {
        return HashMap::new();
    }

    let mut by_class_name: HashMap<String, u64> = HashMap::new();
    for entry in WalkDir::new(&build_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() || !is_generated_binding_watcher_path(path) {
            continue;
        }
        let Some(class_name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(modified_at_secs) = std::fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
        else {
            continue;
        };
        match by_class_name.get(class_name) {
            Some(existing) if *existing >= modified_at_secs => {}
            _ => {
                by_class_name.insert(class_name.to_string(), modified_at_secs);
            }
        }
    }
    by_class_name
}

#[cfg(test)]
#[path = "databinding_watcher_tests.rs"]
mod tests;
