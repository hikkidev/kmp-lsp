//! Server-side poll watcher for AGP-generated ViewBinding classes under `build/`.
//!
//! Editors' native file watchers typically skip gitignored paths, so generated
//! `*Binding.java` files are re-discovered here after Gradle builds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::mpsc;
use walkdir::WalkDir;

use crate::indexer::{
    is_generated_binding_watcher_path, DatabindingWatcherHandle, DatabindingWatcherState, Indexer,
};
use crate::workspace::Event;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Per-class binding file metadata for change detection (mtime + size).
#[derive(Debug, Clone, PartialEq, Eq)]
struct BindingFileSnapshot {
    modified_at_secs: u64,
    modified_at_nanos: u32,
    file_size: u64,
}

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
        let mut snapshots: HashMap<PathBuf, HashMap<String, BindingFileSnapshot>> = HashMap::new();
        let mut databinding_dirs: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let mut interval = tokio::time::interval(poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let module_roots = poll_state.registered_module_roots();
            for module_root in module_roots {
                let dirs = resolve_databinding_dirs(&mut databinding_dirs, &module_root);
                let module_root_for_blocking = module_root.clone();
                let dirs_for_blocking = dirs.clone();
                let current_snapshot = tokio::task::spawn_blocking(move || {
                    snapshot_databinding_binding_files(
                        &module_root_for_blocking,
                        &dirs_for_blocking,
                    )
                })
                .await
                .unwrap_or_default();

                let changed = match snapshots.get(&module_root) {
                    Some(previous) => previous != &current_snapshot,
                    None => snapshot_differs_from_index(&indexer, &module_root, &current_snapshot),
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

/// True when on-disk bindings differ from the side index (first poll baseline).
fn snapshot_differs_from_index(
    indexer: &Indexer,
    module_root: &Path,
    current_snapshot: &HashMap<String, BindingFileSnapshot>,
) -> bool {
    if current_snapshot.is_empty() {
        return false;
    }
    match indexer.generated_bindings.get(module_root) {
        Some(module_bindings) => {
            if module_bindings.entries.len() != current_snapshot.len() {
                return true;
            }
            for (class_name, entry) in &module_bindings.entries {
                let Some(snapshot) = current_snapshot.get(class_name) else {
                    return true;
                };
                if snapshot.modified_at_secs != entry.modified_at_secs {
                    return true;
                }
            }
            false
        }
        None => true,
    }
}

/// Resolve cached databinding dirs for `module_root`, re-discovering when the
/// cache is empty and `build/` now exists (first poll may run before Gradle).
fn resolve_databinding_dirs(
    databinding_dirs: &mut HashMap<PathBuf, Vec<PathBuf>>,
    module_root: &Path,
) -> Vec<PathBuf> {
    let build_dir = module_root.join("build");
    if let Some(cached) = databinding_dirs.get(module_root) {
        if !cached.is_empty() {
            return cached.clone();
        }
        if !build_dir.is_dir() {
            return Vec::new();
        }
    } else if !build_dir.is_dir() {
        return Vec::new();
    }

    let discovered = discover_databinding_dirs(module_root);
    if !discovered.is_empty() {
        databinding_dirs.insert(module_root.to_path_buf(), discovered.clone());
    }
    discovered
}

/// Discover `databinding` directories under `<module>/build/`.
fn discover_databinding_dirs(module_root: &Path) -> Vec<PathBuf> {
    let build_dir = module_root.join("build");
    if !build_dir.is_dir() {
        return Vec::new();
    }

    let mut dirs = Vec::new();
    for entry in WalkDir::new(&build_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            let Some(name) = entry.file_name().to_str() else {
                return true;
            };
            !matches!(name, "intermediates" | "tmp" | "kotlin")
        })
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        if entry.file_name() == "databinding" {
            dirs.push(entry.path().to_path_buf());
        }
    }
    dirs
}

/// Snapshot class name → file metadata for `*Binding.java` under discovered databinding dirs.
fn snapshot_databinding_binding_files(
    module_root: &Path,
    databinding_dirs: &[PathBuf],
) -> HashMap<String, BindingFileSnapshot> {
    let mut by_class_name: HashMap<String, BindingFileSnapshot> = HashMap::new();
    if databinding_dirs.is_empty() {
        return by_class_name;
    }

    for databinding_dir in databinding_dirs {
        for entry in WalkDir::new(databinding_dir)
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
            let Some(metadata) = std::fs::metadata(path).ok() else {
                continue;
            };
            let modified = metadata.modified().ok();
            let modified_at_secs = modified
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let modified_at_nanos = modified
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.subsec_nanos())
                .unwrap_or(0);
            let file_size = metadata.len();
            let candidate = BindingFileSnapshot {
                modified_at_secs,
                modified_at_nanos,
                file_size,
            };
            match by_class_name.get(class_name) {
                Some(existing) if snapshot_is_newer_or_equal(existing, &candidate) => {}
                _ => {
                    by_class_name.insert(class_name.to_string(), candidate);
                }
            }
        }
    }

    // Fallback: if build layout changed and dirs were stale, refresh discovery once.
    if by_class_name.is_empty() {
        let refreshed = discover_databinding_dirs(module_root);
        if refreshed.len() != databinding_dirs.len() {
            return snapshot_databinding_binding_files(module_root, &refreshed);
        }
    }

    by_class_name
}

fn snapshot_is_newer_or_equal(
    existing: &BindingFileSnapshot,
    candidate: &BindingFileSnapshot,
) -> bool {
    if existing.modified_at_secs != candidate.modified_at_secs {
        return existing.modified_at_secs > candidate.modified_at_secs;
    }
    if existing.modified_at_nanos != candidate.modified_at_nanos {
        return existing.modified_at_nanos > candidate.modified_at_nanos;
    }
    existing.file_size >= candidate.file_size
}

#[cfg(test)]
#[path = "databinding_watcher_tests.rs"]
mod tests;
