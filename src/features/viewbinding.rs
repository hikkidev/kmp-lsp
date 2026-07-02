//! ViewBinding navigation — post-resolution remap, hover, references (PR 4–5).

use std::cell::RefCell;
use std::path::Path;

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Range, SymbolKind, Url};
use tree_sitter::{Node, Parser, Tree};

use crate::backend::cursor::CursorContext;
use crate::backend::format::format_contextual_hover;
use crate::features::definition::locs_to_opt_response;
use crate::features::references::find_references_with_qualifier;
use crate::features::traits::{DocumentAccess, SymbolIndex};
use crate::indexer::live_tree::{lang_for_path, parse_live, utf16_col_to_byte};
use crate::indexer::NodeExt;
use crate::indexer::{
    binding_class_name_for_layout, binding_field_name_to_id, binding_id_to_field_name,
    find_this_context_in_lines, is_layout_xml_path, layout_name_for_binding_class,
    module_root_for_generated_file, module_root_for_source_file, IndexRead, Indexer, ThisContext,
};
use crate::inlay_hints::{line_starts, ts_byte_col_to_utf16};
use crate::queries::{
    KIND_NAV_EXPR, KIND_SIMPLE_IDENT, KIND_THIS_EXPR, KIND_XML_ATT_VALUE, KIND_XML_DOCUMENT,
    KIND_XML_ELEMENT, KIND_XML_EMPTY_ELEM_TAG, KIND_XML_NAME, KIND_XML_STAG,
};
use crate::resolver::{
    infer::infer_field_chain_type, infer_receiver_type, infer_receiver_type_at, ReceiverKind,
    ReceiverType,
};
use crate::types::{CursorPos, FileData, SymbolEntry};
use crate::StrExt;

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

/// Convert an LSP `Position` (UTF-16 column) into a tree-sitter `Point`
/// (byte column) for `content`. tree-sitter `Point.column` is a byte offset, so
/// passing `position.character` unconverted misplaces the cursor on any line
/// with a multi-byte character before it — matching the Kotlin paths that
/// already convert via `utf16_col_to_byte`.
fn xml_point_for_position(content: &str, position: Position) -> tree_sitter::Point {
    let line_text = content
        .split('\n')
        .nth(position.line as usize)
        .unwrap_or("");
    tree_sitter::Point {
        row: position.line as usize,
        column: utf16_col_to_byte(line_text, position.character as usize),
    }
}

fn view_id_reference_at_position(content: &str, position: Position) -> Option<String> {
    XML_NAV_PARSER.with(|cell| {
        let tree = cell.borrow_mut().parse(content, None)?;
        let root = tree.root_node();
        if root.kind() != KIND_XML_DOCUMENT {
            return None;
        }
        let target_point = xml_point_for_position(content, position);
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
        let target_point = xml_point_for_position(content, position);
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

// ─── Binding-field hover (PR 5) ──────────────────────────────────────────────

/// Strip package prefix from a type name (`android.widget.TextView` → `TextView`).
pub(crate) fn short_type_name(type_name: &str) -> String {
    type_name
        .trim()
        .rsplit('.')
        .next()
        .unwrap_or(type_name)
        .to_string()
}

/// Kotlin-style hover for a generated binding field: `val title: TextView` / `val title: TextView?`.
pub(crate) fn format_binding_field_hover(
    field_name: &str,
    type_name: &str,
    nullable: bool,
) -> String {
    let short = short_type_name(type_name);
    let rendered_type = if nullable { format!("{short}?") } else { short };
    let signature = format!("val {field_name}: {rendered_type}");
    format_contextual_hover(&signature, ".kt", None)
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

/// Kotlin-style hover for a field on a known generated binding class, resolved
/// as seen from the source file at `uri` (import package first, then the file's
/// own module, then a workspace-unique match).
pub(crate) fn binding_field_hover_for_class(
    index: &Indexer,
    uri: &Url,
    class_name: &str,
    field_name: &str,
) -> Option<String> {
    let binding_file_uri = binding_file_uri_for_source(index, uri, class_name)?;
    let file_data = index.file_data_for(&binding_file_uri)?;
    let symbol = file_data.symbols.iter().find(|symbol| {
        symbol.name == field_name
            && matches!(
                symbol.kind,
                SymbolKind::FIELD | SymbolKind::PROPERTY | SymbolKind::VARIABLE
            )
    })?;
    let type_name = java_field_type_from_detail(&symbol.detail, field_name)?;
    Some(format_binding_field_hover(
        field_name,
        &type_name,
        symbol.nullable,
    ))
}

/// Resolve the generated binding file for `class_name` as seen from the source
/// file at `uri`, so multi-module workspaces with same-named binding classes
/// pick the module the file actually refers to.
fn binding_file_uri_for_source(index: &Indexer, uri: &Url, class_name: &str) -> Option<String> {
    if let Some(imported) = binding_file_uri_from_import(index, uri, class_name) {
        return Some(imported);
    }
    if let Some(own_module) = binding_file_uri_in_own_module(index, uri, class_name) {
        return Some(own_module);
    }
    binding_file_uri_if_unambiguous(index, class_name)
}

/// Match the source file's import of `class_name` against each discovered
/// binding's Java package.
fn binding_file_uri_from_import(index: &Indexer, uri: &Url, class_name: &str) -> Option<String> {
    let file_data = index.file_data_for(uri.as_str())?;
    let class_suffix = format!(".{class_name}");
    let import = file_data.imports.iter().find(|import| {
        !import.is_star
            && import.local_name == class_name
            && import.full_path.ends_with(&class_suffix)
    })?;
    let (import_package, _class) = import.full_path.rsplit_once('.')?;
    for module in index.generated_bindings.iter() {
        let Some(entry) = module.value().entries.get(class_name) else {
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

fn binding_file_uri_in_own_module(index: &Indexer, uri: &Url, class_name: &str) -> Option<String> {
    let path = uri.to_file_path().ok()?;
    let module_root = module_root_for_source_file(&path)?;
    let module = index.generated_bindings.get(&module_root)?;
    let entry = module.entries.get(class_name)?;
    Some(entry.file_uri.clone())
}

/// Fall back to the single workspace-wide match; `None` when the class name is
/// ambiguous across modules (a wrong-module answer is worse than no answer).
fn binding_file_uri_if_unambiguous(index: &Indexer, class_name: &str) -> Option<String> {
    let mut unique_match: Option<String> = None;
    for module in index.generated_bindings.iter() {
        let Some(entry) = module.value().entries.get(class_name) else {
            continue;
        };
        if unique_match.is_some() {
            return None;
        }
        unique_match = Some(entry.file_uri.clone());
    }
    unique_match
}

/// When `location` is a generated binding field, return Kotlin-style hover markdown.
pub(crate) fn binding_field_hover_at_location<I: IndexRead>(
    index: &I,
    location: &Location,
    field_name: &str,
) -> Option<String> {
    if !index.is_generated_binding_uri(location.uri.as_str()) {
        return None;
    }
    let file_data = index.get_file_data(location.uri.as_str())?;
    let symbol = file_data
        .symbols
        .iter()
        .find(|symbol| {
            symbol.name == field_name
                && matches!(
                    symbol.kind,
                    SymbolKind::FIELD | SymbolKind::PROPERTY | SymbolKind::VARIABLE
                )
        })
        .or_else(|| symbol_at_location(&file_data, location))?;
    if symbol.name != field_name {
        return None;
    }
    if !matches!(
        symbol.kind,
        SymbolKind::FIELD | SymbolKind::PROPERTY | SymbolKind::VARIABLE
    ) {
        return None;
    }
    let type_name = java_field_type_from_detail(&symbol.detail, field_name)?;
    Some(format_binding_field_hover(
        field_name,
        &type_name,
        symbol.nullable,
    ))
}

// ─── Receiver-verified references (PR 5) ─────────────────────────────────────

/// Resolve the expected `*Binding` class for a references request at `position`.
pub(crate) fn resolve_expected_binding_class(
    index: &Indexer,
    uri: &Url,
    position: Position,
    ctx: &CursorContext,
) -> Option<String> {
    if index.is_generated_binding_uri(uri.as_str()) {
        if !ctx.word.starts_with_uppercase() {
            return binding_class_from_file_uri(index, uri);
        }
        return None;
    }

    if let Some(receiver_type) = ctx.contextual.as_ref() {
        if let Some(class_name) = binding_class_from_receiver_type(receiver_type) {
            return Some(class_name);
        }
    }

    if let Some(qualifier) = ctx.qualifier.as_deref() {
        if qualifier.contains('.') {
            let segments: Vec<&str> = qualifier.split('.').collect();
            if let Some(class_name) =
                binding_class_for_receiver_chain(index, uri, position, &segments)
            {
                return Some(class_name);
            }
            let chain: Vec<String> = segments
                .iter()
                .map(|segment| (*segment).to_string())
                .collect();
            if let Some(receiver_type) = infer_field_chain_type(index, &chain, uri) {
                return binding_class_from_receiver_type(&receiver_type);
            }
        }
        let receiver_type = infer_receiver_type_at(index, qualifier, uri, position)?;
        return binding_class_from_receiver_type(&receiver_type);
    }

    None
}

fn binding_class_from_file_uri(index: &Indexer, uri: &Url) -> Option<String> {
    let file_data = index.get_file_data(uri.as_str())?;
    binding_class_from_file_data(&file_data)
}

fn binding_class_from_receiver_type(receiver_type: &ReceiverType) -> Option<String> {
    if receiver_type.leaf.ends_with("Binding") {
        return Some(receiver_type.leaf.clone());
    }
    if receiver_type.qualified.ends_with("Binding") {
        return Some(receiver_type.leaf.clone());
    }
    None
}

/// Walk a receiver chain through generated binding Java fields (`binding.header` →
/// `ViewHeaderBinding`) using the importing source file for module pairing.
fn binding_class_for_receiver_chain(
    index: &Indexer,
    uri: &Url,
    position: Position,
    segments: &[&str],
) -> Option<String> {
    if segments.is_empty() {
        return None;
    }
    let root_type = infer_receiver_type_at(index, segments[0], uri, position)?;
    let mut binding_class = binding_class_from_receiver_type(&root_type)?;
    for field in &segments[1..] {
        let field_type = java_binding_field_type(index, uri, &binding_class, field)?;
        if !field_type.ends_with("Binding") {
            return None;
        }
        binding_class = field_type;
    }
    Some(binding_class)
}

fn java_binding_field_type(
    index: &Indexer,
    uri: &Url,
    class_name: &str,
    field_name: &str,
) -> Option<String> {
    let binding_file_uri = binding_file_uri_for_source(index, uri, class_name)?;
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

fn receiver_matches_binding_class(
    receiver_type: &ReceiverType,
    expected_binding_class: &str,
) -> bool {
    receiver_type.leaf == expected_binding_class
        || receiver_type.qualified == expected_binding_class
        || receiver_type
            .qualified
            .ends_with(&format!(".{expected_binding_class}"))
}

/// Find Kotlin usages of a binding field, verified by receiver type.
pub(crate) async fn find_binding_field_references(
    index: &Indexer,
    expected_binding_class: &str,
    field_name: &str,
    uri: &Url,
    line: u32,
    include_decl: bool,
) -> Vec<Location> {
    let candidates =
        find_references_with_qualifier(field_name, None, uri, line, include_decl, index).await;

    candidates
        .into_iter()
        .filter(|location| !index.is_generated_binding_uri(location.uri.as_str()))
        .filter(|location| {
            verify_binding_field_reference(index, location, field_name, expected_binding_class)
        })
        .collect()
}

fn verify_binding_field_reference(
    index: &Indexer,
    location: &Location,
    field_name: &str,
    expected_binding_class: &str,
) -> bool {
    let Some((tree, bytes)) = live_or_disk_tree(index, &location.uri) else {
        return false;
    };
    let receiver_type = if let Some(navigation_node) =
        navigation_expression_at_position(&tree, &bytes, location.range.start, field_name)
    {
        infer_receiver_for_navigation(index, &navigation_node, &bytes, &location.uri)
    } else {
        implicit_receiver_type_for_bare_field(index, &tree, &bytes, location, field_name)
    };
    let Some(receiver_type) = receiver_type else {
        return false;
    };
    receiver_matches_binding_class(&receiver_type, expected_binding_class)
}

fn implicit_receiver_type_for_bare_field(
    index: &Indexer,
    tree: &Tree,
    bytes: &[u8],
    location: &Location,
    field_name: &str,
) -> Option<ReceiverType> {
    let root = tree.root_node();
    let target_point = tree_sitter::Point {
        row: location.range.start.line as usize,
        column: location.range.start.character as usize,
    };
    let node = root.descendant_for_point_range(target_point, target_point)?;
    if node.kind() != KIND_SIMPLE_IDENT {
        return None;
    }
    if node.utf8_text_owned(bytes).as_deref() != Some(field_name) {
        return None;
    }
    // A local val/var/param named `field_name` shadows the binding member, so a
    // bare usage here is not a binding-field reference.
    if index.name_shadowed_by_local_declaration(
        &location.uri,
        location.range.start.line as usize,
        location.range.start.character as usize,
        field_name,
    ) {
        return None;
    }
    let lines = index.mem_lines_for(location.uri.as_str())?;
    let this_context = find_this_context_in_lines(
        &lines,
        CursorPos {
            line: location.range.start.line as usize,
            utf16_col: location.range.start.character as usize,
        },
        index,
        &location.uri,
    );
    match this_context {
        ThisContext::Resolved(resolved_type) => Some(ReceiverType::from_raw(resolved_type)),
        ThisContext::InsideReceiver | ThisContext::NotFound => None,
    }
}

fn live_or_disk_tree(index: &Indexer, uri: &Url) -> Option<(Tree, Vec<u8>)> {
    if let Some(document) = index.live_doc(uri) {
        return Some((document.tree.clone(), document.bytes.clone()));
    }
    let path = uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    let language = lang_for_path(uri.path())?;
    let document = parse_live(&content, language)?;
    Some((document.tree, document.bytes))
}

fn navigation_expression_at_position<'tree>(
    tree: &'tree Tree,
    bytes: &[u8],
    position: Position,
    field_name: &str,
) -> Option<Node<'tree>> {
    let root = tree.root_node();
    let target_point = tree_sitter::Point {
        row: position.line as usize,
        column: position.character as usize,
    };
    let mut node = root.descendant_for_point_range(target_point, target_point)?;
    loop {
        if node.kind() == KIND_NAV_EXPR
            && navigation_member_name(&node, bytes)?.as_str() == field_name
        {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn navigation_member_name(navigation_node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let named_count = navigation_node.named_child_count();
    if named_count < 2 {
        return None;
    }
    let suffix_node = navigation_node.named_child(named_count - 1)?;
    suffix_node
        .first_child_of_kind(KIND_SIMPLE_IDENT)?
        .utf8_text_owned(bytes)
}

fn infer_receiver_for_navigation(
    index: &Indexer,
    navigation_node: &Node<'_>,
    bytes: &[u8],
    uri: &Url,
) -> Option<ReceiverType> {
    let named_count = navigation_node.named_child_count();
    if named_count < 2 {
        return None;
    }
    let receiver_node = navigation_node.named_child(0)?;
    let suffix_node = navigation_node.named_child(named_count - 1)?;
    let operator = suffix_node.child(0)?;
    if operator.kind() != "." {
        return None;
    }
    infer_receiver_type_for_node(index, &receiver_node, bytes, uri)
}

fn infer_receiver_type_for_node(
    index: &Indexer,
    receiver_node: &Node<'_>,
    bytes: &[u8],
    uri: &Url,
) -> Option<ReceiverType> {
    match receiver_node.kind() {
        KIND_THIS_EXPR => infer_contextual_receiver_type(index, receiver_node, bytes, uri, "this"),
        KIND_SIMPLE_IDENT => {
            let name = receiver_node.utf8_text_owned(bytes)?;
            if name == "super" {
                return None;
            }
            if name == "this" || name == "it" {
                return infer_contextual_receiver_type(index, receiver_node, bytes, uri, &name);
            }
            infer_receiver_type(index, ReceiverKind::Variable(&name), uri)
        }
        KIND_NAV_EXPR => {
            let chain = pure_field_chain(receiver_node, bytes)?;
            if chain.len() < 2 || chain[0] == "this" || chain[0] == "super" {
                return None;
            }
            if let Some(receiver_type) = infer_field_chain_type(index, &chain, uri) {
                return Some(receiver_type);
            }
            let start = receiver_node.start_position();
            let line_start_offsets = line_starts(bytes);
            let utf16_column =
                ts_byte_col_to_utf16(bytes, &line_start_offsets, start.row, start.column);
            let position = Position::new(start.row as u32, utf16_column as u32);
            let segments: Vec<&str> = chain.iter().map(String::as_str).collect();
            binding_class_for_receiver_chain(index, uri, position, &segments)
                .map(ReceiverType::from_raw)
        }
        _ => None,
    }
}

/// Infer the type of a contextual receiver (`this` in `binding.apply { this.title }`,
/// `it` in `binding.also { it.title }`) via lambda-scope analysis at the
/// receiver token's position.
fn infer_contextual_receiver_type(
    index: &Indexer,
    receiver_node: &Node<'_>,
    bytes: &[u8],
    uri: &Url,
    contextual_name: &str,
) -> Option<ReceiverType> {
    let start = receiver_node.start_position();
    let line_start_offsets = line_starts(bytes);
    let utf16_column = ts_byte_col_to_utf16(bytes, &line_start_offsets, start.row, start.column);
    let position = Position::new(start.row as u32, utf16_column as u32);
    infer_receiver_type(
        index,
        ReceiverKind::Contextual {
            name: contextual_name,
            position,
        },
        uri,
    )
}

fn pure_field_chain(receiver_node: &Node<'_>, bytes: &[u8]) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = *receiver_node;
    loop {
        if current.kind() == KIND_SIMPLE_IDENT {
            segments.insert(0, current.utf8_text_owned(bytes)?);
            break;
        }
        if current.kind() != KIND_NAV_EXPR {
            return None;
        }
        let named_count = current.named_child_count();
        if named_count < 2 {
            return None;
        }
        let suffix_node = current.named_child(named_count - 1)?;
        if suffix_node.child(0)?.kind() != "." {
            return None;
        }
        let member = suffix_node.first_child_of_kind(KIND_SIMPLE_IDENT)?;
        segments.insert(0, member.utf8_text_owned(bytes)?);
        current = current.named_child(0)?;
    }
    Some(segments)
}

/// References from `@+id/field` in a layout XML file.
pub(crate) async fn find_layout_xml_references(
    index: &Indexer,
    uri: &Url,
    position: Position,
    include_decl: bool,
) -> Option<Vec<Location>> {
    let path = uri.to_file_path().ok()?;
    if !is_layout_xml_path(&path) {
        return None;
    }
    let layout_data = index.layout_data_for_uri(uri.as_str())?;
    let content = layout_content_for_uri(index, uri)?;
    let view_id = view_id_reference_at_position(&content, position)?;
    let field_name = binding_id_to_field_name(&view_id);
    let expected_class = binding_class_name_for_layout(&layout_data.layout_name);
    let decl_position = id_attribute_position(&content, &view_id)?;
    Some(
        find_binding_field_references(
            index,
            &expected_class,
            &field_name,
            uri,
            decl_position.line,
            include_decl,
        )
        .await,
    )
}

fn id_attribute_position(content: &str, view_id: &str) -> Option<Position> {
    let needle = format!("@+id/{view_id}");
    let offset = content
        .find(&needle)
        .or_else(|| content.find(&format!("@id/{view_id}")))?;
    let mut line = 0_u32;
    let mut character = 0_u32;
    for (index, character_value) in content.char_indices() {
        if index == offset {
            return Some(Position { line, character });
        }
        if character_value == '\n' {
            line += 1;
            character = 0;
        } else {
            character += character_value.len_utf16() as u32;
        }
    }
    None
}

/// Shared binding-class resolution for staleness diagnostics (PR 6).
pub(crate) fn binding_class_for_field_access(
    index: &Indexer,
    receiver_node: &Node<'_>,
    bytes: &[u8],
    uri: &Url,
) -> Option<String> {
    let receiver_type = infer_receiver_type_for_node(index, receiver_node, bytes, uri)?;
    binding_class_from_receiver_type(&receiver_type)
}

/// Binding class of the implicit `this` receiver at a bare member usage
/// (`title` inside `with(binding) { title }`). Returns `None` when the bare name
/// is not an implicit binding-field access — including when a local
/// declaration shadows it. The bare-member counterpart of
/// `binding_class_for_field_access`.
pub(crate) fn binding_class_for_bare_field_access(
    index: &Indexer,
    identifier_node: &Node<'_>,
    field_name: &str,
    bytes: &[u8],
    uri: &Url,
) -> Option<String> {
    let start = identifier_node.start_position();
    let line_start_offsets = line_starts(bytes);
    let utf16_column = ts_byte_col_to_utf16(bytes, &line_start_offsets, start.row, start.column);
    if index.name_shadowed_by_local_declaration(uri, start.row, utf16_column, field_name) {
        return None;
    }
    let lines = index.mem_lines_for(uri.as_str())?;
    let this_context = find_this_context_in_lines(
        &lines,
        CursorPos {
            line: start.row,
            utf16_col: utf16_column,
        },
        index,
        uri,
    );
    let receiver_type = match this_context {
        ThisContext::Resolved(resolved_type) => ReceiverType::from_raw(resolved_type),
        ThisContext::InsideReceiver | ThisContext::NotFound => return None,
    };
    binding_class_from_receiver_type(&receiver_type)
}

/// Whether a binding field's id still exists in any live layout variant.
pub(crate) fn view_id_live_for_binding_field(
    index: &Indexer,
    module_root: &Path,
    layout_name: &str,
    field_name: &str,
) -> bool {
    let view_id = binding_field_name_to_id(field_name);
    if !index
        .layouts_declaring_view_id(module_root, layout_name, &view_id)
        .is_empty()
    {
        return true;
    }
    !index
        .include_tag_for_field(module_root, layout_name, field_name)
        .is_empty()
}

#[cfg(test)]
#[path = "viewbinding_tests.rs"]
mod tests;
