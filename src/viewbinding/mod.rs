//! ViewBinding side index, discovery, field types, and navigation helpers.

pub(crate) mod diagnostics;
pub(crate) mod discovery;
pub(crate) mod field_type;
pub(crate) mod hover;
pub(crate) mod index;
pub(crate) mod layout;
pub(crate) mod navigation;
pub(crate) mod receiver;
pub(crate) mod state;
pub(crate) mod watcher;

pub(crate) use diagnostics::{stale_binding_field_diagnostics, viewbinding_import_diagnostics};
pub(crate) use discovery::{
    binding_class_name_for_layout, binding_field_name_to_id, binding_id_to_field_name,
    discover_databinding_dirs, import_triggers_binding_discovery,
    is_generated_binding_watcher_path, is_view_binding_class_name, layout_name_for_binding_class,
    module_root_for_generated_file, module_root_for_source_file, spawn_binding_discovery_worker,
    view_id_matches_lookup, BindingDiscoveryHandle, DatabindingWatcherHandle,
    DatabindingWatcherState, GeneratedBindingClassLocation, ModuleBindings,
    ModuleBindingsCacheEntry,
};
pub(crate) use field_type::{
    binding_field_type, binding_layout_completion_fields, infer_bare_binding_field_type,
    java_field_type_from_detail, short_type_name,
};
pub(crate) use hover::{binding_field_access_hover, fallback_local_binding_hover};
pub(crate) use index::ViewBindingIndex;
pub(crate) use layout::{
    element_tag_at_layout_position, id_attribute_position_for_view_id, is_layout_xml_path,
    layout_path_components, spawn_layout_indexing_worker, view_id_at_layout_position,
    LayoutCacheEntry, LayoutFileData, LayoutIndexingHandle,
};
pub(crate) use navigation::{
    binding_field_hover_at_location, binding_field_hover_for_class,
    binding_field_in_generated_java, binding_field_in_live_layout,
    binding_field_in_live_layout_by_name, find_binding_field_definition,
    find_binding_field_references, find_binding_implementation, find_layout_xml_definition,
    find_layout_xml_implementation, find_layout_xml_references, format_binding_field_hover,
    normalize_reference_location_to_utf16_for_test, remap_generated_binding_definitions,
    resolve_expected_binding_class,
};
pub(crate) use receiver::{
    bare_member_exists_on_binding_receiver, binding_class_for_bare_field_access,
    binding_class_for_field_access, binding_class_for_receiver_chain,
    binding_class_from_receiver_type, implicit_receiver_type_for_bare_member_at,
    receiver_matches_binding_class, receiver_type_for_binding_field_reference,
};
pub(crate) use state::ViewBindingState;
pub(crate) use watcher::{spawn_databinding_watcher, spawn_databinding_watcher_with_interval};
