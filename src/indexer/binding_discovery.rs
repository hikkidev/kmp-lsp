//! AGP-generated ViewBinding class discovery and layout pairing helpers.
//!
//! Scans `<module>/build/` for `*Binding.java` files whose `package` ends in
//! `.databinding`, indexes them through the normal Java pipeline, and maintains
//! a per-module side index for query-time layout↔class pairing (PR 4+).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use dashmap::DashSet;
use tokio::sync::mpsc;
use tower_lsp::lsp_types::Url;
use walkdir::WalkDir;

use crate::indexer::layout::LayoutFileData;
use crate::types::ImportEntry;

// ─── Data types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GeneratedBindingEntry {
    pub class_name: String,
    pub file_uri: String,
    /// File mtime (seconds since Unix epoch) when the entry was discovered.
    pub modified_at_secs: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ModuleBindings {
    pub entries: HashMap<String, GeneratedBindingEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ModuleBindingsCacheEntry {
    pub entries: HashMap<String, GeneratedBindingEntry>,
}

// ─── Pure path helpers ────────────────────────────────────────────────────────

/// Derive the Android module root from a generated binding path:
/// `<X>/build/.../FooBarBinding.java` → `<X>`.
pub(crate) fn module_root_for_generated_file(path: &Path) -> Option<PathBuf> {
    let components: Vec<Component<'_>> = path.components().collect();
    let build_index = components.iter().position(|component| {
        matches!(component, Component::Normal(name) if name.to_str() == Some("build"))
    })?;
    if build_index == 0 {
        return None;
    }
    Some(components[..build_index].iter().collect())
}

/// Derive the Android module root from a Kotlin/Java source path:
/// `<X>/src/<sourceset>/...` → `<X>`.
pub(crate) fn module_root_for_source_file(path: &Path) -> Option<PathBuf> {
    let components: Vec<Component<'_>> = path.components().collect();
    let source_index = components.iter().position(|component| {
        matches!(component, Component::Normal(name) if name.to_str() == Some("src"))
    })?;
    if source_index == 0 {
        return None;
    }
    Some(components[..source_index].iter().collect())
}

/// True when `path` matches the best-effort watcher pattern for generated bindings:
/// under a `build/` segment, filename `*Binding.java`, and a `databinding` path segment.
pub(crate) fn is_generated_binding_watcher_path(path: &Path) -> bool {
    if !is_binding_java_filename(path) {
        return false;
    }
    let mut has_build = false;
    let mut has_databinding = false;
    for component in path.components() {
        if let Component::Normal(name) = component {
            match name.to_str() {
                Some("build") => has_build = true,
                Some("databinding") => has_databinding = true,
                _ => {}
            }
        }
    }
    has_build && has_databinding
}

fn is_binding_java_filename(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    file_name.ends_with("Binding.java")
}

// ─── Name mapping (AGP ViewBinding conventions) ───────────────────────────────

/// Layout file name → generated binding class name (`foo_bar` → `FooBarBinding`).
#[allow(dead_code)] // PR 4 navigation
pub(crate) fn binding_class_name_for_layout(layout_name: &str) -> String {
    format!("{}Binding", snake_case_to_pascal_case(layout_name))
}

/// Generated binding class name → layout file name (`FooBarBinding` → `foo_bar`).
#[allow(dead_code)] // PR 4 navigation
pub(crate) fn layout_name_for_binding_class(class_name: &str) -> Option<String> {
    let base = class_name.strip_suffix("Binding")?;
    if base.is_empty() {
        return None;
    }
    Some(pascal_case_to_snake_case(base))
}

fn snake_case_to_pascal_case(name: &str) -> String {
    name.split('_')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut characters = segment.chars();
            match characters.next() {
                None => String::new(),
                Some(first) => first
                    .to_uppercase()
                    .chain(characters.flat_map(char::to_lowercase))
                    .collect(),
            }
        })
        .collect()
}

fn pascal_case_to_snake_case(name: &str) -> String {
    let mut result = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_uppercase() && index > 0 {
            result.push('_');
        }
        result.extend(character.to_lowercase());
    }
    result
}

// ─── Package verification ─────────────────────────────────────────────────────

fn package_from_java_source(content: &str) -> Option<String> {
    for line in content.lines().take(30) {
        let trimmed = line.trim();
        if let Some(package_name) = trimmed.strip_prefix("package ") {
            let package_name = package_name.trim_end_matches(';').trim();
            if !package_name.is_empty() {
                return Some(package_name.to_string());
            }
        }
    }
    None
}

fn is_databinding_package(package_name: &str) -> bool {
    package_name.ends_with(".databinding")
}

fn read_binding_java_metadata(path: &Path) -> Option<(String, String, u64)> {
    let content = std::fs::read_to_string(path).ok()?;
    let package_name = package_from_java_source(&content)?;
    if !is_databinding_package(&package_name) {
        return None;
    }
    let class_name = path.file_stem()?.to_str()?.to_string();
    let modified_at_secs = std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let file_uri = Url::from_file_path(path).ok()?.to_string();
    Some((class_name, file_uri, modified_at_secs))
}

// ─── Discovery ────────────────────────────────────────────────────────────────

/// Walk `<module_root>/build/` for AGP-generated `*Binding.java` files.
///
/// Keeps the newest mtime per class name across build variants. Ignores files
/// with the wrong package or that cannot be read.
pub(crate) fn discover_generated_bindings(module_root: &Path) -> Vec<GeneratedBindingEntry> {
    let build_dir = module_root.join("build");
    if !build_dir.is_dir() {
        return Vec::new();
    }

    let mut by_class_name: HashMap<String, GeneratedBindingEntry> = HashMap::new();
    for entry in WalkDir::new(&build_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() || !is_binding_java_filename(path) {
            continue;
        }
        let Some((class_name, file_uri, modified_at_secs)) = read_binding_java_metadata(path)
        else {
            continue;
        };
        let candidate = GeneratedBindingEntry {
            class_name: class_name.clone(),
            file_uri,
            modified_at_secs,
        };
        match by_class_name.get(&class_name) {
            Some(existing) if existing.modified_at_secs >= modified_at_secs => {}
            _ => {
                by_class_name.insert(class_name, candidate);
            }
        }
    }

    by_class_name.into_values().collect()
}

// ─── Import trigger ───────────────────────────────────────────────────────────

/// True when an import path matches `*.databinding.*Binding` (non-star).
pub(crate) fn import_triggers_binding_discovery(import_path: &str) -> bool {
    if !import_path.ends_with("Binding") {
        return false;
    }
    let segments: Vec<&str> = import_path.split('.').collect();
    if segments.len() < 2 {
        return false;
    }
    segments.get(segments.len() - 2) == Some(&"databinding")
}

pub(crate) fn file_imports_trigger_binding_discovery(imports: &[ImportEntry]) -> bool {
    imports.iter().any(|entry| {
        !entry.is_star && import_triggers_binding_discovery(&entry.full_path)
    })
}

// ─── Background worker ────────────────────────────────────────────────────────

struct BindingDiscoveryRequest {
    module_root: PathBuf,
}

/// Cheap handle for enqueueing per-module generated binding discovery.
#[derive(Clone)]
pub(crate) struct BindingDiscoveryHandle {
    tx: Option<mpsc::UnboundedSender<BindingDiscoveryRequest>>,
    in_flight: Arc<DashSet<PathBuf>>,
}

impl BindingDiscoveryHandle {
    pub(crate) fn noop() -> Self {
        Self {
            tx: None,
            in_flight: Arc::new(DashSet::new()),
        }
    }

    /// Enqueue discovery for `module_root`. Duplicate in-flight requests are coalesced.
    pub(crate) fn request(&self, module_root: PathBuf) {
        let Some(ref sender) = self.tx else {
            return;
        };
        if !self.in_flight.insert(module_root.clone()) {
            return;
        }
        let _ = sender.send(BindingDiscoveryRequest { module_root });
    }

    pub(crate) fn clear(&self) {
        self.in_flight.clear();
    }
}

/// Spawn the background binding-discovery worker. Returns a handle for hot-path callers.
pub(crate) fn spawn_binding_discovery_worker(indexer: Arc<super::Indexer>) -> BindingDiscoveryHandle {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let in_flight = Arc::new(DashSet::new());
    let handle = BindingDiscoveryHandle {
        tx: Some(sender),
        in_flight: Arc::clone(&in_flight),
    };
    tokio::spawn(async move {
        while let Some(request) = receiver.recv().await {
            let module_root = request.module_root;
            let indexer = Arc::clone(&indexer);
            let in_flight = Arc::clone(&in_flight);
            tokio::task::spawn_blocking(move || {
                indexer.index_generated_bindings(&module_root);
                in_flight.remove(&module_root);
            })
            .await
            .ok();
        }
    });
    handle
}

// ─── Indexer integration ─────────────────────────────────────────────────────

impl super::Indexer {
    pub(crate) fn set_binding_discovery_handle(&self, handle: BindingDiscoveryHandle) {
        if let Ok(mut guard) = self.binding_discovery.write() {
            *guard = handle;
        }
    }

    pub(crate) fn request_generated_binding_discovery(&self, module_root: PathBuf) {
        if let Ok(handle) = self.binding_discovery.read() {
            handle.request(module_root);
        }
    }

    /// Discover generated bindings for `module_root`, update the side index, and
    /// feed each file through the normal Java indexer.
    ///
    /// Idempotent and additive — safe to call repeatedly.
    ///
    /// PR 3 extension point: register `module_root` with the server-side databinding
    /// poll watcher here via `watch_module`.
    pub(crate) fn index_generated_bindings(&self, module_root: &Path) {
        let discovered = discover_generated_bindings(module_root);
        let entries: HashMap<String, GeneratedBindingEntry> = discovered
            .into_iter()
            .map(|entry| (entry.class_name.clone(), entry))
            .collect();
        self.generated_bindings.insert(
            module_root.to_path_buf(),
            Arc::new(ModuleBindings {
                entries: entries.clone(),
            }),
        );

        for entry in entries.values() {
            let Ok(uri) = Url::parse(&entry.file_uri) else {
                continue;
            };
            if let Ok(path) = uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(&uri, &content);
                }
            }
        }
    }

    pub(crate) fn maybe_enqueue_binding_discovery_for_file(
        &self,
        uri: &Url,
        imports: &[ImportEntry],
    ) {
        if !file_imports_trigger_binding_discovery(imports) {
            return;
        }
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        let Some(module_root) = module_root_for_source_file(&path) else {
            return;
        };
        self.request_generated_binding_discovery(module_root);
    }

    /// Layout variants for a binding class in the given module, default variant first.
    #[allow(dead_code)] // PR 4 navigation
    pub(crate) fn layouts_for_binding_class(
        &self,
        class_name: &str,
        module_root: &Path,
    ) -> Vec<Arc<LayoutFileData>> {
        let Some(layout_name) = layout_name_for_binding_class(class_name) else {
            return Vec::new();
        };
        let mut layouts: Vec<Arc<LayoutFileData>> = self
            .layouts
            .iter()
            .filter_map(|entry| {
                let data = entry.value();
                if data.module_root.as_path() == module_root && data.layout_name == layout_name {
                    Some(Arc::clone(data))
                } else {
                    None
                }
            })
            .collect();
        layouts.sort_by(|left, right| {
            match (
                left.variant_qualifier.is_empty(),
                right.variant_qualifier.is_empty(),
            ) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left
                    .variant_qualifier
                    .cmp(&right.variant_qualifier),
            }
        });
        layouts
    }

    /// True when `uri` is a discovered generated binding file (side-index membership).
    #[allow(dead_code)] // PR 4 remap predicate
    pub(crate) fn is_generated_binding_uri(&self, uri: &str) -> bool {
        self.generated_bindings.iter().any(|module| {
            module
                .value()
                .entries
                .values()
                .any(|entry| entry.file_uri == uri)
        })
    }

    pub(crate) fn restore_generated_bindings_from_cache(
        &self,
        cached: &HashMap<String, ModuleBindingsCacheEntry>,
    ) {
        for (module_root_string, cache_entry) in cached {
            let module_root = PathBuf::from(module_root_string);
            self.generated_bindings.insert(
                module_root.clone(),
                Arc::new(ModuleBindings {
                    entries: cache_entry.entries.clone(),
                }),
            );
            for entry in cache_entry.entries.values() {
                let Ok(uri) = Url::parse(&entry.file_uri) else {
                    continue;
                };
                if let Ok(path) = uri.to_file_path() {
                    if path.exists() {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            self.index_content(&uri, &content);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "binding_discovery_tests.rs"]
mod tests;
