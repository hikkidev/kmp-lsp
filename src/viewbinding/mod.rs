//! ViewBinding side index, discovery, field types, and navigation helpers.

pub(crate) mod discovery;
pub(crate) mod field_type;
pub(crate) mod index;
pub(crate) mod layout;
pub(crate) mod state;

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
pub(crate) use index::ViewBindingIndex;
pub(crate) use layout::{
    element_tag_at_layout_position, id_attribute_position_for_view_id, is_layout_xml_path,
    layout_path_components, spawn_layout_indexing_worker, view_id_at_layout_position,
    LayoutCacheEntry, LayoutFileData, LayoutIndexingHandle,
};
pub(crate) use state::ViewBindingState;
