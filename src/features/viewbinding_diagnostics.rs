//! ViewBinding diagnostics: build-required import warning, viewBindingIgnore, staleness.

use tower_lsp::lsp_types::*;

use crate::indexer::live_tree::LiveDoc;
use crate::indexer::{
    import_triggers_binding_discovery, layout_name_for_binding_class, module_root_for_source_file,
    Indexer, NodeExt,
};
use crate::queries::{KIND_NAV_EXPR, KIND_SIMPLE_IDENT};
use crate::Language;
use crate::StrExt;

use super::viewbinding::{binding_class_for_field_access, view_id_live_for_binding_field};

const DIAGNOSTIC_SOURCE: &str = "kmp-lsp";

/// Warn on databinding imports when the paired layout exists but binding generation is missing or opted out.
pub(crate) fn viewbinding_import_diagnostics(index: &Indexer, uri: &Url) -> Vec<Diagnostic> {
    if !matches!(Language::from_path(uri.path()), Language::Kotlin | Language::Java) {
        return Vec::new();
    }

    let Some(file_data) = index.file_data_for(uri.as_str()) else {
        return Vec::new();
    };
    let Some(module_root) = uri
        .to_file_path()
        .ok()
        .and_then(|path| module_root_for_source_file(&path))
    else {
        return Vec::new();
    };

    let mut diagnostics = Vec::new();
    for import in &file_data.imports {
        if import.is_star || !import_triggers_binding_discovery(&import.full_path) {
            continue;
        }
        let class_name = import.local_name.clone();
        let Some(layout_name) = layout_name_for_binding_class(&class_name) else {
            continue;
        };
        if !index.layout_exists_for_binding(&module_root, &layout_name) {
            continue;
        }

        let message = if index.any_layout_variant_ignores_view_binding(&module_root, &layout_name)
        {
            "Layout opts out of ViewBinding (`tools:viewBindingIgnore`)".to_string()
        } else if !index.generated_binding_discovered(&module_root, &class_name) {
            "ViewBinding class not generated — build the project".to_string()
        } else {
            continue;
        };

        if let Some(range) = import_line_range(&file_data.lines, &import.full_path) {
            diagnostics.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                message,
                ..Default::default()
            });
        }
    }
    diagnostics
}

fn import_line_range(lines: &[String], import_path: &str) -> Option<Range> {
    let needle = format!("import {import_path}");
    for (line_index, line) in lines.iter().enumerate() {
        if line.contains(&needle) {
            let end_col = line.chars().map(|character| character.len_utf16() as u32).sum();
            return Some(Range {
                start: Position {
                    line: line_index as u32,
                    character: 0,
                },
                end: Position {
                    line: line_index as u32,
                    character: end_col,
                },
            });
        }
    }
    None
}

/// Information diagnostics on stale binding field usages (id gone from all layout variants).
pub(crate) fn stale_binding_field_diagnostics(
    index: &Indexer,
    uri: &Url,
    document: &LiveDoc,
) -> Vec<Diagnostic> {
    if !matches!(Language::from_path(uri.path()), Language::Kotlin) {
        return Vec::new();
    }

    let bytes = &document.bytes;
    let mut diagnostics = Vec::new();
    collect_stale_binding_fields(
        document.tree.root_node(),
        bytes,
        index,
        uri,
        &mut diagnostics,
    );
    diagnostics
}

fn collect_stale_binding_fields(
    node: tree_sitter::Node,
    bytes: &[u8],
    index: &Indexer,
    uri: &Url,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node.kind() == KIND_NAV_EXPR {
        if let Some(diagnostic) = check_stale_binding_field(&node, bytes, index, uri) {
            diagnostics.push(diagnostic);
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_stale_binding_fields(cursor.node(), bytes, index, uri, diagnostics);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn check_stale_binding_field(
    navigation_node: &tree_sitter::Node,
    bytes: &[u8],
    index: &Indexer,
    uri: &Url,
) -> Option<Diagnostic> {
    let named_count = navigation_node.named_child_count();
    if named_count < 2 {
        return None;
    }
    let receiver_node = navigation_node.named_child(0)?;
    let suffix_node = navigation_node.named_child(named_count - 1)?;
    if suffix_node.child(0)?.kind() != "." {
        return None;
    }
    let field_node = suffix_node.first_child_of_kind(KIND_SIMPLE_IDENT)?;
    let field_name = field_node.utf8_text_owned(bytes)?;
    if field_name.starts_with_uppercase() {
        return None;
    }

    let binding_class = binding_class_for_field_access(index, &receiver_node, bytes, uri)?;
    let module_root = uri
        .to_file_path()
        .ok()
        .and_then(|path| module_root_for_source_file(&path))?;
    let layout_name = layout_name_for_binding_class(&binding_class)?;

    if !binding_field_exists(index, &binding_class, &field_name) {
        return None;
    }
    if view_id_live_for_binding_field(index, &module_root, &layout_name, &field_name) {
        return None;
    }

    Some(Diagnostic {
        range: node_to_range(field_node),
        severity: Some(DiagnosticSeverity::INFORMATION),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!(
            "Field `{field_name}` comes from a stale build; id no longer in layout"
        ),
        ..Default::default()
    })
}

fn binding_field_exists(index: &Indexer, binding_class: &str, field_name: &str) -> bool {
    for module in index.generated_bindings.iter() {
        for entry in module.value().entries.values() {
            if entry.class_name != binding_class {
                continue;
            }
            let Some(file_data) = index.file_data_for(&entry.file_uri) else {
                continue;
            };
            if file_data.symbols.iter().any(|symbol| {
                symbol.name == field_name
                    && matches!(
                        symbol.kind,
                        SymbolKind::FIELD | SymbolKind::PROPERTY | SymbolKind::VARIABLE
                    )
            }) {
                return true;
            }
        }
    }
    false
}

fn node_to_range(node: tree_sitter::Node) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start: Position {
            line: start.row as u32,
            character: start.column as u32,
        },
        end: Position {
            line: end.row as u32,
            character: end.column as u32,
        },
    }
}

#[cfg(test)]
#[path = "viewbinding_diagnostics_tests.rs"]
mod tests;
