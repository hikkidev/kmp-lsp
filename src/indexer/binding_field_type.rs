//! ViewBinding field type resolution from layout XML (primary) with generated Java fallback.

use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{SymbolKind, Url};

use super::{
    binding_class_name_for_layout, binding_field_name_to_id, layout_name_for_binding_class,
    module_root_for_generated_file, module_root_for_source_file, view_id_matches_lookup, Indexer,
    LayoutFileData,
};

/// Resolve a ViewBinding field type as seen from `source_uri`.
///
/// Layout XML is consulted first; generated `*Binding.java` is the fallback.
pub(crate) fn binding_field_type(
    index: &Indexer,
    source_uri: Option<&Url>,
    binding_class: &str,
    field_name: &str,
) -> Option<String> {
    if !binding_class.ends_with("Binding") {
        return None;
    }
    if let Some(module_root) = module_root_for_binding_class(index, source_uri, binding_class) {
        if let Some(layout_name) = layout_name_for_binding_class(binding_class) {
            if let Some(field_type) =
                field_type_from_layout_variants(index, &module_root, &layout_name, field_name)
            {
                return Some(field_type);
            }
        }
    }
    source_uri.and_then(|uri| java_binding_field_type(index, uri, binding_class, field_name))
}

/// Extract the Java field type from a `SymbolEntry.detail` string.
pub(crate) fn java_field_type_from_detail(detail: &str, field_name: &str) -> Option<String> {
    const MODIFIERS: &[&str] = &["public", "private", "protected", "final", "static"];
    let without_name = detail
        .trim()
        .trim_end_matches(';')
        .strip_suffix(field_name)?
        .trim();
    let type_tokens: Vec<&str> = without_name
        .split_whitespace()
        .filter(|token| {
            !MODIFIERS.contains(token) && !token.starts_with('@') && !token.ends_with(';')
        })
        .collect();
    type_tokens.last().map(|token| token.to_string())
}

/// Strip package prefix from a type name (`android.widget.TextView` → `TextView`).
pub(crate) fn short_type_name(type_name: &str) -> String {
    type_name
        .trim()
        .rsplit('.')
        .next()
        .unwrap_or(type_name)
        .to_string()
}

pub(crate) fn java_binding_field_type(
    index: &Indexer,
    source_uri: &Url,
    binding_class: &str,
    field_name: &str,
) -> Option<String> {
    let binding_file_uri = binding_file_uri_for_source(index, source_uri, binding_class)?;
    let file_data = index.file_data_for(&binding_file_uri)?;
    let symbol = file_data.symbols.iter().find(|symbol| {
        symbol.name == field_name
            && matches!(
                symbol.kind,
                SymbolKind::FIELD | SymbolKind::PROPERTY | SymbolKind::VARIABLE
            )
    })?;
    java_field_type_from_detail(&symbol.detail, field_name)
}

fn field_type_from_layout_variants(
    index: &Indexer,
    module_root: &Path,
    layout_name: &str,
    field_name: &str,
) -> Option<String> {
    let layouts =
        index.layouts_for_binding_class(&binding_class_name_for_layout(layout_name), module_root);
    if layouts.is_empty() {
        return None;
    }

    if field_name == "root" {
        return root_field_type_from_variants(&layouts);
    }

    if let Some(include_type) = include_field_type_from_variants(&layouts, field_name) {
        return Some(include_type);
    }

    view_id_field_type_from_variants(&layouts, field_name)
}

fn view_id_field_type_from_variants(
    layouts: &[std::sync::Arc<LayoutFileData>],
    field_name: &str,
) -> Option<String> {
    let lookup_id = binding_field_name_to_id(field_name);
    let mut tag_names: Vec<String> = Vec::new();
    for layout in layouts {
        for view_id in &layout.view_ids {
            if view_id_matches_lookup(&view_id.id, &lookup_id) {
                tag_names.push(leaf_tag_name(&view_id.tag_name));
            }
        }
    }
    consensus_tag_type(&tag_names)
}

fn include_field_type_from_variants(
    layouts: &[std::sync::Arc<LayoutFileData>],
    field_name: &str,
) -> Option<String> {
    let lookup_id = binding_field_name_to_id(field_name);
    for layout in layouts {
        for include in &layout.includes {
            if include
                .id
                .as_deref()
                .is_some_and(|include_id| view_id_matches_lookup(include_id, &lookup_id))
            {
                return Some(binding_class_name_for_layout(&include.included_layout_name));
            }
        }
    }
    None
}

fn root_field_type_from_variants(layouts: &[std::sync::Arc<LayoutFileData>]) -> Option<String> {
    let tag_names: Vec<String> = layouts
        .iter()
        .map(|layout| leaf_tag_name(&layout.root_tag.tag_name))
        .collect();
    consensus_tag_type(&tag_names)
}

fn consensus_tag_type(tag_names: &[String]) -> Option<String> {
    if tag_names.is_empty() {
        return None;
    }
    let first = &tag_names[0];
    if tag_names.iter().all(|tag| tag == first) {
        Some(first.clone())
    } else {
        Some("View".to_string())
    }
}

fn leaf_tag_name(tag_name: &str) -> String {
    short_type_name(tag_name)
}

fn module_root_for_binding_class(
    index: &Indexer,
    source_uri: Option<&Url>,
    binding_class: &str,
) -> Option<PathBuf> {
    if let Some(uri) = source_uri {
        if let Some(binding_uri) = binding_file_uri_from_import(index, uri, binding_class) {
            if let Some(path) = Url::parse(&binding_uri)
                .ok()
                .and_then(|parsed| parsed.to_file_path().ok())
            {
                if let Some(module_root) = module_root_for_generated_file(&path) {
                    return Some(module_root);
                }
            }
        }
        if let Ok(path) = uri.to_file_path() {
            if let Some(module_root) = module_root_for_source_file(&path) {
                if module_has_binding_layout(index, &module_root, binding_class) {
                    return Some(module_root);
                }
            }
        }
    }
    module_root_if_unambiguous_layout(index, binding_class)
}

fn module_has_binding_layout(index: &Indexer, module_root: &Path, binding_class: &str) -> bool {
    let Some(layout_name) = layout_name_for_binding_class(binding_class) else {
        return false;
    };
    index.layout_exists_for_binding(module_root, &layout_name)
        || index.generated_binding_discovered(module_root, binding_class)
}

fn module_root_if_unambiguous_layout(index: &Indexer, binding_class: &str) -> Option<PathBuf> {
    let layout_name = layout_name_for_binding_class(binding_class)?;
    let mut unique_match: Option<PathBuf> = None;
    for entry in index.layouts.iter() {
        let data = entry.value();
        if data.layout_name != layout_name {
            continue;
        }
        if unique_match
            .as_ref()
            .is_some_and(|existing| existing != data.module_root.as_path())
        {
            return None;
        }
        unique_match = Some(data.module_root.clone());
    }
    unique_match
}

fn binding_file_uri_for_source(
    index: &Indexer,
    source_uri: &Url,
    binding_class: &str,
) -> Option<String> {
    if let Some(imported) = binding_file_uri_from_import(index, source_uri, binding_class) {
        return Some(imported);
    }
    if let Some(own_module) = binding_file_uri_in_own_module(index, source_uri, binding_class) {
        return Some(own_module);
    }
    binding_file_uri_if_unambiguous(index, binding_class)
}

fn binding_file_uri_from_import(
    index: &Indexer,
    source_uri: &Url,
    binding_class: &str,
) -> Option<String> {
    let file_data = index.file_data_for(source_uri.as_str())?;
    let class_suffix = format!(".{binding_class}");
    let import = file_data.imports.iter().find(|import| {
        !import.is_star
            && import.local_name == binding_class
            && import.full_path.ends_with(&class_suffix)
    })?;
    let (import_package, _class) = import.full_path.rsplit_once('.')?;
    for module in index.generated_bindings.iter() {
        let Some(entry) = module.value().entries.get(binding_class) else {
            continue;
        };
        let Some(binding_file_data) = index.file_data_for(&entry.file_uri) else {
            continue;
        };
        if binding_file_data.package.as_deref() == Some(import_package) {
            return Some(entry.file_uri.clone());
        }
    }
    None
}

fn binding_file_uri_in_own_module(
    index: &Indexer,
    source_uri: &Url,
    binding_class: &str,
) -> Option<String> {
    let path = source_uri.to_file_path().ok()?;
    let module_root = module_root_for_source_file(&path)?;
    let module = index.generated_bindings.get(&module_root)?;
    let entry = module.entries.get(binding_class)?;
    Some(entry.file_uri.clone())
}

fn binding_file_uri_if_unambiguous(index: &Indexer, binding_class: &str) -> Option<String> {
    let mut unique_match: Option<String> = None;
    for module in index.generated_bindings.iter() {
        let Some(entry) = module.value().entries.get(binding_class) else {
            continue;
        };
        if unique_match.is_some() {
            return None;
        }
        unique_match = Some(entry.file_uri.clone());
    }
    unique_match
}

#[cfg(test)]
#[path = "binding_field_type_tests.rs"]
mod tests;
