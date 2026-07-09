//! Android layout XML indexing for ViewBinding navigation.
//!
//! Parses `res/layout*/*.xml` files into a side index (`LayoutFileData`) holding
//! view ids, includes, and the root tag metadata needed by later PRs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Parser};

use crate::indexer::NodeExt;
use crate::inlay_hints::{line_starts, ts_byte_col_to_utf16};
use crate::queries::{
    KIND_XML_ATTRIBUTE, KIND_XML_ATT_VALUE, KIND_XML_CONTENT, KIND_XML_DOCUMENT, KIND_XML_ELEMENT,
    KIND_XML_EMPTY_ELEM_TAG, KIND_XML_NAME, KIND_XML_STAG,
};

// ─── Public data types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TagLocation {
    pub tag_name: String,
    pub range: Range,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LayoutViewId {
    pub id: String,
    pub tag_name: String,
    pub tag_range: Range,
    pub id_attribute_range: Range,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LayoutInclude {
    pub id: Option<String>,
    pub included_layout_name: String,
    pub tag_range: Range,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LayoutCacheEntry {
    pub(crate) mtime_secs: u64,
    pub(crate) file_size: u64,
    pub(crate) data: std::sync::Arc<LayoutFileData>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LayoutFileData {
    pub module_root: PathBuf,
    pub layout_name: String,
    pub variant_qualifier: String,
    pub root_tag: TagLocation,
    pub view_binding_ignore: bool,
    pub view_ids: Vec<LayoutViewId>,
    pub includes: Vec<LayoutInclude>,
    #[serde(default)]
    pub element_tags: Vec<TagLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayoutPathComponents {
    pub module_root: PathBuf,
    pub layout_name: String,
    pub variant_qualifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedLayout {
    pub root_tag: Option<TagLocation>,
    pub view_binding_ignore: bool,
    pub view_ids: Vec<LayoutViewId>,
    pub includes: Vec<LayoutInclude>,
    pub element_tags: Vec<TagLocation>,
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

/// Returns true when `path` is an Android layout XML under `res/layout*` or
/// `res/layout-<qualifier>/`.
pub(crate) fn is_layout_xml_path(path: &Path) -> bool {
    layout_path_components(path).is_some()
}

/// Derive module root, layout name, and variant qualifier from a layout file path.
///
/// Expected shape: `<module>/src/<sourceset>/res*/layout*/<name>.xml`
pub(crate) fn layout_path_components(path: &Path) -> Option<LayoutPathComponents> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("xml") {
        return None;
    }

    let components: Vec<Component<'_>> = path.components().collect();
    // Use the last `src` segment so paths like `~/src/project/app/src/main/...` anchor
    // at the module root (`app`), not the parent directory named `src`.
    let source_index = components.iter().rposition(
        |component| matches!(component, Component::Normal(name) if name.to_str() == Some("src")),
    )?;
    if source_index == 0 {
        return None;
    }

    let file_index = components.len().saturating_sub(1);
    if source_index + 3 > file_index {
        return None;
    }

    let module_root: PathBuf = components[..source_index].iter().collect();
    let layout_name = path.file_stem()?.to_str()?.to_string();

    for index in (source_index + 1)..file_index {
        let resource_segment = components.get(index)?;
        let layout_segment = components.get(index + 1)?;
        let resource_name = resource_segment.as_os_str().to_str()?;
        let layout_dir = layout_segment.as_os_str().to_str()?;

        if !resource_name.starts_with("res") {
            continue;
        }

        let variant_qualifier = if layout_dir == "layout" {
            String::new()
        } else if let Some(qualifier) = layout_dir.strip_prefix("layout-") {
            qualifier.to_string()
        } else {
            continue;
        };

        return Some(LayoutPathComponents {
            module_root,
            layout_name,
            variant_qualifier,
        });
    }

    None
}

// ─── XML parsing ──────────────────────────────────────────────────────────────

thread_local! {
    static XML_PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_xml::language_xml())
            .expect("tree-sitter-xml language");
        parser
    });
}

/// Parse layout XML content into view ids, includes, and root-tag metadata.
/// Malformed XML returns a best-effort partial result without panicking.
pub(crate) fn parse_layout_xml(content: &str) -> ParsedLayout {
    let mut parsed = ParsedLayout::default();
    let bytes = content.as_bytes();

    let tree = match XML_PARSER.with(|cell| cell.borrow_mut().parse(content, None)) {
        Some(tree) => tree,
        None => return parsed,
    };

    let root = tree.root_node();
    if root.kind() != KIND_XML_DOCUMENT {
        return parsed;
    }

    let document_element = root
        .child_by_field_name("root")
        .or_else(|| root.first_child_of_kind(KIND_XML_ELEMENT));
    let Some(document_element) = document_element else {
        return parsed;
    };

    walk_element(
        document_element,
        bytes,
        true,
        LayoutWalkState {
            root_tag: &mut parsed.root_tag,
            view_binding_ignore: &mut parsed.view_binding_ignore,
            view_ids: &mut parsed.view_ids,
            includes: &mut parsed.includes,
            element_tags: &mut parsed.element_tags,
        },
    );
    parsed
}

#[derive(Debug)]
struct LayoutWalkState<'a> {
    root_tag: &'a mut Option<TagLocation>,
    view_binding_ignore: &'a mut bool,
    view_ids: &'a mut Vec<LayoutViewId>,
    includes: &'a mut Vec<LayoutInclude>,
    element_tags: &'a mut Vec<TagLocation>,
}

fn walk_element(element: Node<'_>, bytes: &[u8], is_root: bool, state: LayoutWalkState<'_>) {
    let LayoutWalkState {
        root_tag,
        view_binding_ignore,
        view_ids,
        includes,
        element_tags,
    } = state;
    let Some(tag_node) = element
        .first_child_of_kind(KIND_XML_STAG)
        .or_else(|| element.first_child_of_kind(KIND_XML_EMPTY_ELEM_TAG))
    else {
        return;
    };

    let Some(tag_name) = tag_name_from(tag_node, bytes) else {
        return;
    };
    let tag_range = tree_sitter_range_to_lsp(tag_node.range(), bytes);
    let attributes = collect_attribute_map(tag_node, bytes);

    if is_root {
        *root_tag = Some(TagLocation {
            tag_name: tag_name.clone(),
            range: tag_range,
        });
        if attribute_is_true(&attributes, "tools:viewBindingIgnore") {
            *view_binding_ignore = true;
        }
    } else {
        element_tags.push(TagLocation {
            tag_name: tag_name.clone(),
            range: tag_range,
        });
    }

    if tag_name == "include" {
        if let Some(included_layout_name) = attributes
            .get("layout")
            .map(|(value, _)| value.clone())
            .and_then(|value| parse_layout_reference(&value))
        {
            let include_id = attributes
                .get("android:id")
                .map(|(value, _)| value.clone())
                .and_then(|value| parse_view_id_reference(&value));
            includes.push(LayoutInclude {
                id: include_id,
                included_layout_name,
                tag_range,
            });
        }
    }

    if let Some(id_reference) = attributes
        .get("android:id")
        .map(|(value, _)| value.clone())
        .and_then(|value| parse_view_id_reference(&value))
    {
        let id_attribute_range = attributes
            .get("android:id")
            .map(|(_, range)| *range)
            .unwrap_or(tag_range);
        view_ids.push(LayoutViewId {
            id: id_reference,
            tag_name,
            tag_range,
            id_attribute_range,
        });
    }

    let Some(content_node) = element.first_child_of_kind(KIND_XML_CONTENT) else {
        return;
    };
    let mut cursor = content_node.walk();
    for child in content_node.children(&mut cursor) {
        if child.kind() == KIND_XML_ELEMENT {
            walk_element(
                child,
                bytes,
                false,
                LayoutWalkState {
                    root_tag,
                    view_binding_ignore,
                    view_ids,
                    includes,
                    element_tags,
                },
            );
        }
    }
}

fn tag_name_from(tag_node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let name_node = tag_node.first_child_of_kind(KIND_XML_NAME)?;
    name_node.utf8_text_owned(bytes)
}

fn collect_attribute_map(tag_node: Node<'_>, bytes: &[u8]) -> HashMap<String, (String, Range)> {
    let mut map = HashMap::new();
    let mut cursor = tag_node.walk();
    for child in tag_node.children(&mut cursor) {
        if child.kind() != KIND_XML_ATTRIBUTE {
            continue;
        }
        let Some(name_node) = child.first_child_of_kind(KIND_XML_NAME) else {
            continue;
        };
        let Some(attribute_name) = name_node.utf8_text_owned(bytes) else {
            continue;
        };
        let value_node = child.first_child_of_kind(KIND_XML_ATT_VALUE);
        let value_text = value_node
            .and_then(|node| node.utf8_text_owned(bytes))
            .map(|text| strip_xml_quotes(&text))
            .unwrap_or_default();
        let value_range = value_node
            .map(|node| tree_sitter_range_to_lsp(node.range(), bytes))
            .unwrap_or_else(|| tree_sitter_range_to_lsp(child.range(), bytes));
        map.insert(attribute_name, (value_text, value_range));
    }
    map
}

fn attribute_is_true(attributes: &HashMap<String, (String, Range)>, name: &str) -> bool {
    attributes
        .get(name)
        .is_some_and(|(value, _)| value.eq_ignore_ascii_case("true"))
}

/// Strip surrounding single or double quotes from an XML attribute value.
///
/// Malformed values (e.g. a lone `"`) return the trimmed input without panicking.
pub(crate) fn strip_xml_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_view_id_reference(value: &str) -> Option<String> {
    let stripped = strip_xml_quotes(value);
    let id = stripped.strip_prefix("@+id/")?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

fn parse_layout_reference(value: &str) -> Option<String> {
    let stripped = strip_xml_quotes(value);
    let name = stripped.strip_prefix("@layout/")?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

fn tree_sitter_range_to_lsp(range: tree_sitter::Range, bytes: &[u8]) -> Range {
    let line_start_offsets = line_starts(bytes);
    Range {
        start: Position {
            line: range.start_point.row as u32,
            character: ts_byte_col_to_utf16(
                bytes,
                &line_start_offsets,
                range.start_point.row,
                range.start_point.column,
            ) as u32,
        },
        end: Position {
            line: range.end_point.row as u32,
            character: ts_byte_col_to_utf16(
                bytes,
                &line_start_offsets,
                range.end_point.row,
                range.end_point.column,
            ) as u32,
        },
    }
}

/// Build a [`LayoutFileData`] from path components and parsed XML content.
pub(crate) fn build_layout_file_data(
    components: &LayoutPathComponents,
    parsed: &ParsedLayout,
) -> Option<LayoutFileData> {
    let root_tag = parsed.root_tag.clone()?;
    Some(LayoutFileData {
        module_root: components.module_root.clone(),
        layout_name: components.layout_name.clone(),
        variant_qualifier: components.variant_qualifier.clone(),
        root_tag,
        view_binding_ignore: parsed.view_binding_ignore,
        view_ids: parsed.view_ids.clone(),
        includes: parsed.includes.clone(),
        element_tags: parsed.element_tags.clone(),
    })
}

fn position_in_layout_range(position: Position, range: Range) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

/// Resolve a `@+id/...` reference at `position` from indexed layout metadata.
pub(crate) fn view_id_at_layout_position(
    layout_data: &LayoutFileData,
    position: Position,
) -> Option<String> {
    layout_data
        .view_ids
        .iter()
        .find(|view_id| position_in_layout_range(position, view_id.id_attribute_range))
        .map(|view_id| view_id.id.clone())
}

/// Resolve an element tag name at `position` from indexed layout metadata.
pub(crate) fn element_tag_at_layout_position(
    layout_data: &LayoutFileData,
    position: Position,
) -> Option<String> {
    if position_in_layout_range(position, layout_data.root_tag.range) {
        return Some(layout_data.root_tag.tag_name.clone());
    }
    for include in &layout_data.includes {
        if position_in_layout_range(position, include.tag_range) {
            return Some("include".to_string());
        }
    }
    for element_tag in &layout_data.element_tags {
        if position_in_layout_range(position, element_tag.range) {
            return Some(element_tag.tag_name.clone());
        }
    }
    layout_data
        .view_ids
        .iter()
        .find(|view_id| position_in_layout_range(position, view_id.tag_range))
        .map(|view_id| view_id.tag_name.clone())
}

/// Declaration position for `view_id` from the layout side index.
pub(crate) fn id_attribute_position_for_view_id(
    layout_data: &LayoutFileData,
    view_id: &str,
) -> Option<Position> {
    layout_data
        .view_ids
        .iter()
        .find(|entry| entry.id == view_id)
        .map(|entry| entry.id_attribute_range.start)
}

/// Collect layout XML paths under `<module_root>/src/*/res*/layout*/` without
/// walking unrelated source trees.
fn module_layout_paths(module_root: &Path) -> Vec<PathBuf> {
    let source_root = module_root.join("src");
    if !source_root.is_dir() {
        return Vec::new();
    }

    let mut paths = Vec::new();
    let Ok(source_sets) = std::fs::read_dir(&source_root) else {
        return paths;
    };
    for source_set in source_sets.filter_map(Result::ok) {
        let source_set_path = source_set.path();
        if !source_set_path.is_dir() {
            continue;
        }
        let Ok(resource_dirs) = std::fs::read_dir(&source_set_path) else {
            continue;
        };
        for resource_dir in resource_dirs.filter_map(Result::ok) {
            let resource_path = resource_dir.path();
            if !resource_path.is_dir() {
                continue;
            }
            let Some(resource_name) = resource_path.file_name().and_then(|name| name.to_str())
            else {
                continue;
            };
            if !resource_name.starts_with("res") {
                continue;
            }
            let Ok(layout_dirs) = std::fs::read_dir(&resource_path) else {
                continue;
            };
            for layout_dir in layout_dirs.filter_map(Result::ok) {
                let layout_path = layout_dir.path();
                if !layout_path.is_dir() {
                    continue;
                }
                let Some(layout_dir_name) = layout_path.file_name().and_then(|name| name.to_str())
                else {
                    continue;
                };
                if layout_dir_name != "layout" && !layout_dir_name.starts_with("layout-") {
                    continue;
                }
                let Ok(xml_files) = std::fs::read_dir(&layout_path) else {
                    continue;
                };
                for file in xml_files.filter_map(Result::ok) {
                    let path = file.path();
                    if path.is_file() && is_layout_xml_path(&path) {
                        paths.push(path);
                    }
                }
            }
        }
    }
    paths
}

impl crate::indexer::Indexer {
    pub(crate) fn insert_layout_secondary_index(&self, uri: &str, data: &LayoutFileData) {
        let key = (data.module_root.clone(), data.layout_name.clone());
        let mut entry = self.layouts_by_module_and_name.entry(key).or_default();
        let uri_string = uri.to_string();
        if !entry.contains(&uri_string) {
            entry.push(uri_string);
        }
    }

    pub(crate) fn remove_layout_secondary_index(&self, data: &LayoutFileData, uri: &str) {
        let key = (data.module_root.clone(), data.layout_name.clone());
        if let Some(mut entry) = self.layouts_by_module_and_name.get_mut(&key) {
            entry.retain(|existing| existing != uri);
            if entry.is_empty() {
                drop(entry);
                self.layouts_by_module_and_name.remove(&key);
            }
        }
    }

    /// Index any layout XML files under `module_root` that are not yet in the layout side index.
    ///
    /// Returns how many files were newly indexed. Used on-demand when bulk workspace
    /// discovery missed layouts (gitignore/fd exclusions) but navigation needs them.
    pub(crate) fn ensure_module_layouts_indexed(&self, module_root: &Path) -> usize {
        if self
            .indexing_in_progress
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return 0;
        }
        if self.layouts_indexed_modules.contains(module_root) {
            return 0;
        }

        let layout_paths = module_layout_paths(module_root);
        if layout_paths.is_empty() {
            return 0;
        }

        let mut newly_indexed = 0_usize;
        for path in layout_paths {
            let Ok(uri) = tower_lsp::lsp_types::Url::from_file_path(&path) else {
                continue;
            };
            let uri_string = uri.to_string();
            if self.layouts.contains_key(&uri_string) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            self.index_layout_content(&uri, &content);
            newly_indexed += 1;
        }
        self.layouts_indexed_modules
            .insert(module_root.to_path_buf());
        if newly_indexed > 0 {
            log::debug!(
                "viewbinding: on-demand indexed {newly_indexed} layout(s) under {}",
                module_root.display()
            );
        }
        newly_indexed
    }

    /// Index a single layout XML file into the layout side index.
    pub(crate) fn index_layout_content(&self, uri: &tower_lsp::lsp_types::Url, content: &str) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        let Some(components) = layout_path_components(&path) else {
            return;
        };
        let parsed = parse_layout_xml(content);
        let Some(data) = build_layout_file_data(&components, &parsed) else {
            return;
        };
        let uri_string = uri.to_string();
        let data = std::sync::Arc::new(data);
        self.layouts.insert(uri_string.clone(), Arc::clone(&data));
        self.insert_layout_secondary_index(&uri_string, &data);
        self.request_generated_binding_discovery(components.module_root);
    }
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
