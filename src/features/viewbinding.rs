//! ViewBinding navigation — post-resolution remap and XML-side helpers (PR 4).

use std::cell::RefCell;
use std::path::Path;

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Range, SymbolKind, Url};
use tree_sitter::{Node, Parser};

use crate::backend::cursor::CursorContext;
use crate::features::definition::locs_to_opt_response;
use crate::features::traits::{DocumentAccess, SymbolIndex};
use crate::indexer::NodeExt;
use crate::indexer::{
    binding_field_name_to_id, is_layout_xml_path, layout_name_for_binding_class,
    module_root_for_generated_file, IndexRead,
};
use crate::queries::{
    KIND_XML_ATT_VALUE, KIND_XML_DOCUMENT, KIND_XML_ELEMENT, KIND_XML_EMPTY_ELEM_TAG,
    KIND_XML_NAME, KIND_XML_STAG,
};
use crate::types::{FileData, SymbolEntry};

const ANDROID_TAG_PREFIXES: &[&str] = &["android.widget.", "android.view.", "android.webkit."];

thread_local! {
    static XML_NAV_PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_xml::language_xml())
            .expect("tree-sitter-xml language");
        parser
    });
}

// ─── Kotlin-side post-resolution remap (definition only) ───────────────────────

/// Remap generated `*Binding.java` definition targets to layout XML.
pub(crate) fn remap_generated_binding_definitions<I: IndexRead>(
    index: &I,
    locations: Vec<Location>,
) -> Vec<Location> {
    let mut remapped = Vec::new();
    for location in locations {
        if !index.is_generated_binding_uri(location.uri.as_str()) {
            remapped.push(location);
            continue;
        }
        let Some(targets) = remap_single_binding_location(index, &location) else {
            continue;
        };
        remapped.extend(targets);
    }
    remapped
}

fn remap_single_binding_location<I: IndexRead>(
    index: &I,
    location: &Location,
) -> Option<Vec<Location>> {
    let path = location.uri.to_file_path().ok()?;
    let module_root = module_root_for_generated_file(&path)?;
    let file_data = index.get_file_data(location.uri.as_str())?;
    let symbol = symbol_at_location(&file_data, location).or_else(|| {
        file_data
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::CLASS && symbol.name.ends_with("Binding"))
    })?;

    if symbol.name.ends_with("Binding") {
        if let Some(class_symbol) = file_data
            .symbols
            .iter()
            .find(|entry| entry.kind == SymbolKind::CLASS && entry.name == symbol.name)
        {
            if let Some(layout_targets) = remap_binding_class(index, class_symbol, &module_root) {
                return Some(layout_targets);
            }
        }
    }

    match symbol.kind {
        SymbolKind::CLASS => remap_binding_class(index, symbol, &module_root),
        SymbolKind::FIELD | SymbolKind::PROPERTY | SymbolKind::VARIABLE => {
            remap_binding_field(index, symbol, &module_root, &file_data)
        }
        SymbolKind::METHOD | SymbolKind::FUNCTION if symbol.name == "getRoot" => {
            remap_root_view(index, &module_root, &file_data)
        }
        _ => None,
    }
}

fn remap_binding_class<I: IndexRead>(
    index: &I,
    symbol: &SymbolEntry,
    module_root: &Path,
) -> Option<Vec<Location>> {
    let entries = index.layout_uris_for_binding_class(&symbol.name, module_root);
    if entries.is_empty() {
        return None;
    }
    Some(
        entries
            .into_iter()
            .filter_map(|(uri_string, _data)| {
                location_from_layout_uri(&uri_string).map(layout_file_start_location)
            })
            .collect::<Vec<_>>(),
    )
    .filter(|locations| !locations.is_empty())
}

fn location_from_layout_uri(uri_string: &str) -> Option<Url> {
    Url::parse(uri_string).ok()
}

fn remap_binding_field<I: IndexRead>(
    index: &I,
    symbol: &SymbolEntry,
    module_root: &Path,
    file_data: &FileData,
) -> Option<Vec<Location>> {
    if symbol.name == "rootView" {
        return remap_root_view(index, module_root, file_data);
    }

    let class_name = symbol.container.as_deref()?;
    let layout_name = layout_name_for_binding_class(class_name)?;

    let include_targets = index.include_tag_for_field(module_root, &layout_name, &symbol.name);
    if !include_targets.is_empty() {
        return Some(locations_from_uri_ranges(include_targets));
    }

    let view_id = binding_field_name_to_id(&symbol.name);
    let id_targets = index.layouts_declaring_view_id(module_root, &layout_name, &view_id);
    if id_targets.is_empty() {
        return None;
    }
    Some(locations_from_uri_ranges(id_targets))
}

fn remap_root_view<I: IndexRead>(
    index: &I,
    module_root: &Path,
    file_data: &FileData,
) -> Option<Vec<Location>> {
    let class_name = binding_class_from_file_data(file_data)?;
    let layout_name = layout_name_for_binding_class(&class_name)?;
    let entries = index.layout_uris_for_binding_class(&class_name, module_root);
    let targets: Vec<Location> = entries
        .into_iter()
        .filter(|(_uri, data)| data.layout_name == layout_name)
        .filter_map(|(uri_string, data)| {
            Url::parse(&uri_string).ok().map(|uri| Location {
                uri,
                range: data.root_tag.range,
            })
        })
        .collect();
    if targets.is_empty() {
        None
    } else {
        Some(targets)
    }
}

fn binding_class_from_file_data(file_data: &FileData) -> Option<String> {
    file_data
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::CLASS)
        .map(|symbol| symbol.name.clone())
}

fn layout_file_start_location(uri: Url) -> Location {
    Location {
        uri,
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
    }
}

fn locations_from_uri_ranges(entries: Vec<(String, Range)>) -> Vec<Location> {
    entries
        .into_iter()
        .filter_map(|(uri_string, range)| {
            Url::parse(&uri_string)
                .ok()
                .map(|uri| Location { uri, range })
        })
        .collect()
}

fn symbol_at_location<'a>(file_data: &'a FileData, location: &Location) -> Option<&'a SymbolEntry> {
    if let Some(symbol) = file_data
        .symbols
        .iter()
        .find(|symbol| symbol.selection_range == location.range)
    {
        return Some(symbol);
    }
    if let Some(symbol) = file_data
        .symbols
        .iter()
        .find(|symbol| position_in_range(location.range.start, symbol.selection_range))
    {
        return Some(symbol);
    }
    file_data.symbols.iter().find(|symbol| {
        symbol.kind == SymbolKind::CLASS && position_in_range(location.range.start, symbol.range)
    })
}

fn position_in_range(position: Position, range: Range) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

// ─── Binding-type implementation (Kotlin) ────────────────────────────────────

/// Return the raw generated Java class for a `*Binding` type usage — no remap.
pub(crate) fn find_binding_implementation(
    index: &(impl SymbolIndex + IndexRead),
    ctx: &CursorContext,
    uri: &Url,
    _position: Position,
) -> Option<GotoDefinitionResponse> {
    if !ctx.word.ends_with("Binding") {
        return None;
    }
    let locations = index.find_definition_qualified(&ctx.word, ctx.qualifier.as_deref(), uri);
    let binding_locations: Vec<Location> = locations
        .into_iter()
        .filter(|location| index.is_generated_binding_uri(location.uri.as_str()))
        .collect();
    locs_to_opt_response(binding_locations)
}

// ─── XML-side navigation ─────────────────────────────────────────────────────

/// Definition on `@+id/...` or `@id/...` inside a layout XML file.
pub(crate) fn find_layout_xml_definition(
    index: &(impl IndexRead + DocumentAccess),
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let path = uri.to_file_path().ok()?;
    if !is_layout_xml_path(&path) {
        return None;
    }
    let current = index.layout_data_for_uri(uri.as_str())?;
    let content = layout_content_for_uri(index, uri)?;
    let view_id = view_id_reference_at_position(&content, position)?;
    let declarations = index.layouts_declaring_view_id(
        current.module_root.as_path(),
        &current.layout_name,
        &view_id,
    );
    locs_to_opt_response(locations_from_uri_ranges(declarations))
}

/// Implementation on a layout XML element tag name.
pub(crate) fn find_layout_xml_implementation(
    index: &(impl SymbolIndex + IndexRead + DocumentAccess),
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let path = uri.to_file_path().ok()?;
    if !is_layout_xml_path(&path) {
        return None;
    }
    let content = layout_content_for_uri(index, uri)?;
    let tag_name = element_tag_name_at_position(&content, position)?;
    if tag_name.contains('.') {
        let mut locations = index.qualified_definition_locations(&tag_name);
        if locations.is_empty() {
            locations = index.find_definition_qualified(&tag_name, None, uri);
        }
        if locations.is_empty() {
            if let Some((qualifier, simple)) = tag_name.rsplit_once('.') {
                locations = index.find_definition_qualified(simple, Some(qualifier), uri);
            }
        }
        return locs_to_opt_response(locations);
    }
    for prefix in ANDROID_TAG_PREFIXES {
        let qualified = format!("{prefix}{tag_name}");
        let locations = index.find_definition_qualified(&qualified, None, uri);
        if !locations.is_empty() {
            return locs_to_opt_response(locations);
        }
    }
    None
}

fn layout_content_for_uri(index: &impl DocumentAccess, uri: &Url) -> Option<String> {
    if let Some(lines) = index.mem_lines_for(uri.as_str()) {
        return Some(lines.join("\n"));
    }
    let path = uri.to_file_path().ok()?;
    std::fs::read_to_string(path).ok()
}

fn view_id_reference_at_position(content: &str, position: Position) -> Option<String> {
    XML_NAV_PARSER.with(|cell| {
        let tree = cell.borrow_mut().parse(content, None)?;
        let root = tree.root_node();
        if root.kind() != KIND_XML_DOCUMENT {
            return None;
        }
        let target_point = tree_sitter::Point {
            row: position.line as usize,
            column: position.character as usize,
        };
        let node = root.descendant_for_point_range(target_point, target_point)?;
        if node.kind() != KIND_XML_ATT_VALUE {
            return None;
        }
        let bytes = content.as_bytes();
        let value = node.utf8_text_owned(bytes)?;
        parse_view_id_reference(&value)
    })
}

fn element_tag_name_at_position(content: &str, position: Position) -> Option<String> {
    XML_NAV_PARSER.with(|cell| {
        let tree = cell.borrow_mut().parse(content, None)?;
        let root = tree.root_node();
        if root.kind() != KIND_XML_DOCUMENT {
            return None;
        }
        let target_point = tree_sitter::Point {
            row: position.line as usize,
            column: position.character as usize,
        };
        let mut node = root.descendant_for_point_range(target_point, target_point)?;
        let bytes = content.as_bytes();
        loop {
            if matches!(node.kind(), KIND_XML_STAG | KIND_XML_EMPTY_ELEM_TAG) {
                return tag_name_from(node, bytes);
            }
            if node.kind() == KIND_XML_ELEMENT {
                if let Some(start_tag) = node.first_child_of_kind(KIND_XML_STAG) {
                    return tag_name_from(start_tag, bytes);
                }
                if let Some(empty_tag) = node.first_child_of_kind(KIND_XML_EMPTY_ELEM_TAG) {
                    return tag_name_from(empty_tag, bytes);
                }
            }
            node = node.parent()?;
        }
    })
}

fn tag_name_from(tag_node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let name_node = tag_node.first_child_of_kind(KIND_XML_NAME)?;
    name_node.utf8_text_owned(bytes)
}

fn parse_view_id_reference(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let unquoted = strip_xml_quotes(trimmed);
    if let Some(id) = unquoted.strip_prefix("@+id/") {
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Some(id) = unquoted.strip_prefix("@id/") {
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

fn strip_xml_quotes(value: &str) -> &str {
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

#[cfg(test)]
#[path = "viewbinding_tests.rs"]
mod tests;
