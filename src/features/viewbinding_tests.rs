//! ViewBinding navigation feature tests (PR 4).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::features::definition::find_definition;
use crate::features::hover::compute_hover;
use crate::features::implementation::find_implementation;
use crate::features::traits::SymbolIndex;
use crate::features::viewbinding::{
    binding_field_hover_for_class, find_binding_field_references, find_binding_implementation,
    find_layout_xml_definition, find_layout_xml_implementation, find_layout_xml_references,
    format_binding_field_hover, java_field_type_from_detail, remap_generated_binding_definitions,
    resolve_expected_binding_class, short_type_name,
};
use crate::indexer::{binding_field_name_to_id, binding_id_to_field_name, Indexer};
use crate::parser::nullable_at_line;

const FOO_BAR_LAYOUT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
    android:layout_width="match_parent"
    android:layout_height="match_parent">

    <TextView
        android:id="@+id/title"
        android:layout_width="wrap_content"
        android:layout_height="wrap_content" />

    <TextView
        android:id="@+id/subtitle"
        android:layout_width="wrap_content"
        android:layout_height="wrap_content" />

    <include
        android:id="@+id/header"
        layout="@layout/view_header" />
</LinearLayout>
"#;

const FOO_BAR_LAYOUT_LAND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
    android:layout_width="match_parent"
    android:layout_height="match_parent">

    <TextView
        android:id="@+id/title"
        android:layout_width="wrap_content"
        android:layout_height="wrap_content" />
</LinearLayout>
"#;

const VIEW_HEADER_LAYOUT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
    android:layout_width="match_parent"
    android:layout_height="match_parent">

    <TextView
        android:id="@+id/title"
        android:layout_width="wrap_content"
        android:layout_height="wrap_content" />
</LinearLayout>
"#;

const FOO_BAR_BINDING_JAVA: &str = r#"package com.example.app.databinding;

import android.view.View;
import android.widget.TextView;
import androidx.annotation.Nullable;
import androidx.constraintlayout.widget.ConstraintLayout;

public final class FooBarBinding {
    public final TextView title;
    @Nullable
    public final TextView subtitle;
    public final ViewHeaderBinding header;
    private final ConstraintLayout rootView;

    private FooBarBinding(TextView title, TextView subtitle, ViewHeaderBinding header, ConstraintLayout rootView) {
        this.title = title;
        this.subtitle = subtitle;
        this.header = header;
        this.rootView = rootView;
    }

    public ConstraintLayout getRoot() {
        return rootView;
    }
}
"#;

const VIEW_HEADER_BINDING_JAVA: &str = r#"package com.example.app.databinding;

import android.widget.TextView;

public final class ViewHeaderBinding {
    public final TextView title;
}
"#;

struct ViewBindingFixture {
    _temp: tempfile::TempDir,
    module_root: PathBuf,
    land_layout_uri: Url,
    binding_java_uri: Url,
    kotlin_uri: Url,
    kotlin_source: String,
    indexer: Arc<Indexer>,
}

impl ViewBindingFixture {
    fn build() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let module_root = temp.path().join("app");
        let layout_dir = module_root.join("src/main/res/layout");
        let layout_land_dir = module_root.join("src/main/res/layout-land");
        fs::create_dir_all(&layout_dir).expect("mkdir layout");
        fs::create_dir_all(&layout_land_dir).expect("mkdir layout-land");

        let default_layout_path = layout_dir.join("foo_bar.xml");
        let land_layout_path = layout_land_dir.join("foo_bar.xml");
        let header_layout_path = layout_dir.join("view_header.xml");
        fs::write(&default_layout_path, FOO_BAR_LAYOUT).expect("write default layout");
        fs::write(&land_layout_path, FOO_BAR_LAYOUT_LAND).expect("write land layout");
        fs::write(&header_layout_path, VIEW_HEADER_LAYOUT).expect("write header layout");

        let binding_java_path = module_root.join(
            "build/generated/source/databinding/com/example/app/databinding/FooBarBinding.java",
        );
        let header_binding_path = module_root.join(
            "build/generated/source/databinding/com/example/app/databinding/ViewHeaderBinding.java",
        );
        fs::create_dir_all(binding_java_path.parent().unwrap()).expect("mkdir binding");
        fs::write(&binding_java_path, FOO_BAR_BINDING_JAVA).expect("write binding java");
        fs::write(&header_binding_path, VIEW_HEADER_BINDING_JAVA).expect("write header binding");

        let kotlin_path = module_root.join("src/main/kotlin/com/example/MainActivity.kt");
        fs::create_dir_all(kotlin_path.parent().unwrap()).expect("mkdir kotlin");
        let kotlin_source = r#"package com.example

import com.example.app.databinding.FooBarBinding

class MainActivity {
    fun demo(binding: FooBarBinding) {
        binding.title
        binding.header.title
    }
}
"#;
        fs::write(&kotlin_path, kotlin_source).expect("write kotlin");

        let indexer = Arc::new(Indexer::new());
        indexer.workspace_root.set(temp.path().to_path_buf());

        let default_layout_uri = Url::from_file_path(&default_layout_path).expect("default uri");
        let land_layout_uri = Url::from_file_path(&land_layout_path).expect("land uri");
        let header_layout_uri = Url::from_file_path(&header_layout_path).expect("header uri");
        let binding_java_uri = Url::from_file_path(&binding_java_path).expect("binding uri");
        let kotlin_uri = Url::from_file_path(&kotlin_path).expect("kotlin uri");

        indexer.index_layout_content(&default_layout_uri, FOO_BAR_LAYOUT);
        indexer.index_layout_content(&land_layout_uri, FOO_BAR_LAYOUT_LAND);
        indexer.index_layout_content(&header_layout_uri, VIEW_HEADER_LAYOUT);
        indexer.index_generated_bindings(&module_root);
        indexer.index_content(&kotlin_uri, kotlin_source);
        indexer.set_live_lines(&kotlin_uri, kotlin_source);
        indexer.store_live_tree(&kotlin_uri, kotlin_source);

        Self {
            _temp: temp,
            module_root,
            land_layout_uri,
            binding_java_uri,
            kotlin_uri,
            kotlin_source: kotlin_source.to_string(),
            indexer,
        }
    }

    fn cursor_context(word: &str, qualifier: Option<&str>) -> CursorContext {
        CursorContext {
            word: word.to_string(),
            qualifier: qualifier.map(str::to_string),
            contextual: None,
            lambda_decl: None,
        }
    }

    fn position_in(source: &str, needle: &str) -> Position {
        let offset = source.find(needle).expect("needle in source");
        let mut line = 0_u32;
        let mut character = 0_u32;
        for (index, ch) in source.char_indices() {
            if index == offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                character = 0;
            } else {
                character += ch.len_utf8() as u32;
            }
        }
        Position { line, character }
    }
}

fn response_locations(response: Option<GotoDefinitionResponse>) -> Vec<Location> {
    match response {
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_range,
            })
            .collect(),
        None => Vec::new(),
    }
}

fn uri_path_string(location: &Location) -> String {
    location
        .uri
        .to_file_path()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

#[test]
fn binding_field_name_to_id_inverts_layout_id_mapping() {
    assert_eq!(binding_field_name_to_id("fooBar"), "foo_bar");
    assert_eq!(binding_id_to_field_name("foo_bar"), "fooBar");
    assert_eq!(binding_field_name_to_id("title"), "title");
    assert_eq!(binding_id_to_field_name("title"), "title");
    assert_eq!(binding_field_name_to_id("header"), "header");
}

#[test]
fn short_type_name_strips_package_prefix() {
    assert_eq!(short_type_name("android.widget.TextView"), "TextView");
    assert_eq!(short_type_name("ViewHeaderBinding"), "ViewHeaderBinding");
}

#[test]
fn format_binding_field_hover_renders_nullable_and_non_nullable() {
    let non_null = format_binding_field_hover("title", "android.widget.TextView", false);
    assert!(non_null.contains("val title: TextView"));
    assert!(!non_null.contains("android.widget"));
    let nullable = format_binding_field_hover("subtitle", "TextView", true);
    assert!(nullable.contains("val subtitle: TextView?"));
}

#[test]
fn java_field_type_from_detail_extracts_type() {
    assert_eq!(
        java_field_type_from_detail("public final TextView title", "title"),
        Some("TextView".to_string())
    );
    assert_eq!(
        java_field_type_from_detail("public final ViewHeaderBinding header", "header"),
        Some("ViewHeaderBinding".to_string())
    );
}

#[test]
fn nullable_at_line_detects_annotation_above_field() {
    let lines = vec![
        "import androidx.annotation.Nullable;".to_string(),
        "@Nullable".to_string(),
        "public final TextView subtitle;".to_string(),
    ];
    assert!(nullable_at_line(&lines, 2));
    assert!(!nullable_at_line(
        &["public final TextView title;".to_string()],
        0
    ));
}

#[tokio::test]
async fn hover_on_binding_field_renders_kotlin_style() {
    let fixture = ViewBindingFixture::build();
    let position = ViewBindingFixture::position_in(&fixture.kotlin_source, "binding.title");
    let context = ViewBindingFixture::cursor_context("title", Some("binding"));
    let hover = compute_hover(
        fixture.indexer.as_ref(),
        &context,
        &fixture.kotlin_uri,
        position,
    )
    .expect("hover");
    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(content) => content.value,
        _ => panic!("expected markdown hover"),
    };
    assert!(markdown.contains("val title: TextView"));
    assert!(!markdown.contains("android.widget"));
}

#[tokio::test]
async fn hover_on_nullable_binding_field_shows_question_mark() {
    let fixture = ViewBindingFixture::build();
    let kotlin_source = fixture.kotlin_source.replace(
        "binding.title",
        "binding.subtitle",
    );
    fixture
        .indexer
        .index_content(&fixture.kotlin_uri, &kotlin_source);
    fixture
        .indexer
        .set_live_lines(&fixture.kotlin_uri, &kotlin_source);
    fixture
        .indexer
        .store_live_tree(&fixture.kotlin_uri, &kotlin_source);

    let position = ViewBindingFixture::position_in(&kotlin_source, "binding.subtitle");
    let context = ViewBindingFixture::cursor_context("subtitle", Some("binding"));
    let hover = compute_hover(
        fixture.indexer.as_ref(),
        &context,
        &fixture.kotlin_uri,
        position,
    )
    .expect("hover");
    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(content) => content.value,
        _ => panic!("expected markdown hover"),
    };
    assert!(markdown.contains("val subtitle: TextView?"));
}

#[test]
fn binding_field_hover_for_class_reads_nullable_flag() {
    let fixture = ViewBindingFixture::build();
    let hover = binding_field_hover_for_class(&fixture.indexer, "FooBarBinding", "subtitle")
        .expect("binding hover");
    assert!(hover.contains("TextView?"));
}

#[tokio::test]
async fn binding_field_references_find_qualified_usages() {
    let fixture = ViewBindingFixture::build();
    let position = ViewBindingFixture::position_in(&fixture.kotlin_source, "binding.title");
    let context = ViewBindingFixture::cursor_context("title", Some("binding"));
    let expected = resolve_expected_binding_class(
        &fixture.indexer,
        &fixture.kotlin_uri,
        position,
        &context,
    )
    .expect("expected binding class");
    assert_eq!(expected, "FooBarBinding");

    let references = find_binding_field_references(
        &fixture.indexer,
        &expected,
        "title",
        &fixture.kotlin_uri,
        position.line,
        false,
    )
    .await;
    assert!(!references.is_empty());
    assert!(references
        .iter()
        .all(|location| !fixture.indexer.is_generated_binding_uri(location.uri.as_str())));
}

#[tokio::test]
async fn binding_field_references_exclude_misleading_competitor() {
    let fixture = ViewBindingFixture::build();
    let competitor_path = fixture
        .module_root
        .join("src/main/kotlin/com/example/Competitor.kt");
    fs::create_dir_all(competitor_path.parent().unwrap()).expect("mkdir");
    let competitor_source = r#"package com.example

class Competitor {
    val title: String = "misleading"
}

fun use(competitor: Competitor) {
    competitor.title
}
"#;
    fs::write(&competitor_path, competitor_source).expect("write competitor");
    let competitor_uri = Url::from_file_path(&competitor_path).expect("competitor uri");
    fixture.indexer.index_content(&competitor_uri, competitor_source);

    let position = ViewBindingFixture::position_in(&fixture.kotlin_source, "binding.title");
    let references = find_binding_field_references(
        &fixture.indexer,
        "FooBarBinding",
        "title",
        &fixture.kotlin_uri,
        position.line,
        false,
    )
    .await;
    assert!(references
        .iter()
        .all(|location| !location.uri.as_str().contains("Competitor.kt")));
}

#[tokio::test]
async fn xml_references_match_kotlin_side() {
    let fixture = ViewBindingFixture::build();
    let default_layout_uri = fixture
        .indexer
        .layout_uris_for_binding_class("FooBarBinding", &fixture.module_root)
        .into_iter()
        .find(|(_uri, data)| data.variant_qualifier.is_empty())
        .map(|(uri, _data)| Url::parse(&uri).expect("layout uri"))
        .expect("default layout uri");

    let xml_position = ViewBindingFixture::position_in(FOO_BAR_LAYOUT, "@+id/title");
    let xml_refs = find_layout_xml_references(
        &fixture.indexer,
        &default_layout_uri,
        xml_position,
        false,
    )
    .await
    .expect("xml references");

    let kotlin_position = ViewBindingFixture::position_in(&fixture.kotlin_source, "binding.title");
    let kotlin_refs = find_binding_field_references(
        &fixture.indexer,
        "FooBarBinding",
        "title",
        &fixture.kotlin_uri,
        kotlin_position.line,
        false,
    )
    .await;

    let mut xml_set: Vec<_> = xml_refs
        .iter()
        .map(|location| (location.uri.as_str(), location.range.start.line))
        .collect();
    let mut kotlin_set: Vec<_> = kotlin_refs
        .iter()
        .map(|location| (location.uri.as_str(), location.range.start.line))
        .collect();
    xml_set.sort();
    kotlin_set.sort();
    assert_eq!(xml_set, kotlin_set);
}

#[tokio::test]
async fn definition_on_binding_type_remaps_to_layout_variants_default_first() {
    let fixture = ViewBindingFixture::build();
    let binding_locs =
        fixture
            .indexer
            .find_definition_qualified("FooBarBinding", None, &fixture.kotlin_uri);
    assert!(
        !binding_locs.is_empty(),
        "FooBarBinding must resolve to generated Java before remap"
    );

    let context = ViewBindingFixture::cursor_context("FooBarBinding", None);
    let position =
        ViewBindingFixture::position_in(&fixture.kotlin_source, "binding: FooBarBinding");
    let response = find_definition(&context, &*fixture.indexer, &fixture.kotlin_uri, position)
        .await
        .expect("definition response");
    let locations = response_locations(Some(response));

    assert_eq!(locations.len(), 2);
    assert_eq!(uri_path_string(&locations[0]), "foo_bar.xml");
    assert_eq!(locations[0].range.start.line, 0);
    assert_eq!(locations[0].range.start.character, 0);
    assert!(locations[1].uri.as_str().contains("layout-land"));
}

#[tokio::test]
async fn definition_on_binding_field_remaps_to_view_id_in_all_variants() {
    let fixture = ViewBindingFixture::build();
    let position = ViewBindingFixture::position_in(&fixture.kotlin_source, "binding.title");

    let context = ViewBindingFixture::cursor_context("title", Some("binding"));
    let response = find_definition(&context, &*fixture.indexer, &fixture.kotlin_uri, position)
        .await
        .expect("definition response");
    let locations = response_locations(Some(response));

    assert_eq!(locations.len(), 2);
    assert!(locations
        .iter()
        .all(|location| uri_path_string(location) == "foo_bar.xml"));
    assert!(locations
        .iter()
        .all(|location| location.range.start.line > 0));
}

#[tokio::test]
async fn definition_on_include_field_remaps_to_include_tag() {
    let fixture = ViewBindingFixture::build();
    let position = ViewBindingFixture::position_in(&fixture.kotlin_source, "binding.header");

    let context = ViewBindingFixture::cursor_context("header", Some("binding"));
    let response = find_definition(&context, &*fixture.indexer, &fixture.kotlin_uri, position)
        .await
        .expect("definition response");
    let locations = response_locations(Some(response));

    assert_eq!(locations.len(), 1);
    assert_eq!(uri_path_string(&locations[0]), "foo_bar.xml");
}

#[tokio::test]
async fn definition_on_chained_include_field_remaps_recursively() {
    let fixture = ViewBindingFixture::build();
    let kotlin_source = r#"package com.example

import com.example.app.databinding.ViewHeaderBinding

fun onHeader(header: ViewHeaderBinding) {
    header.title
}
"#;
    let kotlin_path = fixture
        .module_root
        .join("src/main/kotlin/com/example/HeaderUsage.kt");
    fs::write(&kotlin_path, kotlin_source).expect("write header usage");
    let kotlin_uri = Url::from_file_path(&kotlin_path).expect("header usage uri");
    fixture.indexer.index_content(&kotlin_uri, kotlin_source);
    fixture.indexer.set_live_lines(&kotlin_uri, kotlin_source);
    fixture.indexer.store_live_tree(&kotlin_uri, kotlin_source);

    let position = ViewBindingFixture::position_in(kotlin_source, "header.title");
    let context = ViewBindingFixture::cursor_context("title", Some("header"));
    let response = find_definition(&context, &*fixture.indexer, &kotlin_uri, position)
        .await
        .expect("definition response");
    let locations = response_locations(Some(response));

    assert_eq!(locations.len(), 1);
    assert_eq!(uri_path_string(&locations[0]), "view_header.xml");
}

#[tokio::test]
async fn definition_on_get_root_remaps_to_root_tag() {
    let fixture = ViewBindingFixture::build();
    let java_locations =
        fixture
            .indexer
            .find_definition_qualified("getRoot", None, &fixture.binding_java_uri);
    assert!(!java_locations.is_empty());

    let remapped = remap_generated_binding_definitions(&*fixture.indexer, java_locations);
    assert!(!remapped.is_empty());
    assert!(remapped
        .iter()
        .all(|location| uri_path_string(location) == "foo_bar.xml"));
    assert!(remapped[0].range.start.line <= 1);
}

#[tokio::test]
async fn implementation_on_binding_type_returns_raw_java() {
    let fixture = ViewBindingFixture::build();
    let position = ViewBindingFixture::position_in(&fixture.kotlin_source, "FooBarBinding");
    let context = ViewBindingFixture::cursor_context("FooBarBinding", None);

    let response = find_implementation(&context, &*fixture.indexer, &fixture.kotlin_uri, position)
        .await
        .expect("implementation response");
    let locations = response_locations(Some(response));

    assert_eq!(locations.len(), 1);
    assert!(locations[0].uri.as_str().contains("FooBarBinding.java"));
    assert!(locations[0].uri.as_str().contains("build/"));
}

#[test]
fn hand_written_binding_class_is_not_remapped() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let handwritten_path = module_root.join("src/main/java/com/example/FooBarBinding.java");
    fs::create_dir_all(handwritten_path.parent().unwrap()).expect("mkdir java");
    let source = "package com.example;\npublic class FooBarBinding {}\n";
    fs::write(&handwritten_path, source).expect("write handwritten");

    let indexer = Indexer::new();
    let uri = Url::from_file_path(&handwritten_path).expect("uri");
    indexer.index_content(&uri, source);

    let locations = indexer.find_definition_qualified("FooBarBinding", None, &uri);
    let remapped = remap_generated_binding_definitions(&indexer, locations.clone());
    assert_eq!(remapped, locations);
    assert!(!indexer.is_generated_binding_uri(uri.as_str()));
}

#[test]
fn wrong_module_pairing_does_not_remap_to_other_module_layout() {
    let fixture = ViewBindingFixture::build();
    let other_module = fixture.module_root.parent().unwrap().join("other");
    let other_layout = other_module.join("src/main/res/layout/foo_bar.xml");
    fs::create_dir_all(other_layout.parent().unwrap()).expect("mkdir other layout");
    fs::write(&other_layout, FOO_BAR_LAYOUT).expect("write other layout");
    let other_uri = Url::from_file_path(&other_layout).expect("other uri");
    fixture
        .indexer
        .index_layout_content(&other_uri, FOO_BAR_LAYOUT);

    let binding_locations =
        fixture
            .indexer
            .find_definition_qualified("FooBarBinding", None, &fixture.kotlin_uri);
    let remapped = remap_generated_binding_definitions(&*fixture.indexer, binding_locations);

    assert!(remapped
        .iter()
        .all(|location| !location.uri.as_str().contains("/other/")));
}

#[tokio::test]
async fn contextual_receiver_definition_remaps_identically() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let layout_path = module_root.join("src/main/res/layout/foo_bar.xml");
    fs::create_dir_all(layout_path.parent().unwrap()).expect("mkdir");
    fs::write(&layout_path, FOO_BAR_LAYOUT).expect("layout");

    let binding_path = module_root
        .join("build/generated/databinding/com/example/app/databinding/FooBarBinding.java");
    fs::create_dir_all(binding_path.parent().unwrap()).expect("mkdir binding");
    fs::write(&binding_path, FOO_BAR_BINDING_JAVA).expect("binding");

    let kotlin_path = module_root.join("src/main/kotlin/com/example/Context.kt");
    fs::create_dir_all(kotlin_path.parent().unwrap()).expect("mkdir kotlin");
    let kotlin_source = r#"package com.example

import com.example.app.databinding.FooBarBinding

fun withBlock(binding: FooBarBinding) {
    binding.title
}

fun applyBlock(binding: FooBarBinding) {
    binding.apply { it.title }
}

fun runBlock(binding: FooBarBinding) {
    binding.run { it.title }
}
"#;
    fs::write(&kotlin_path, kotlin_source).expect("write kotlin");

    let indexer = Arc::new(Indexer::new());
    let layout_uri = Url::from_file_path(&layout_path).expect("layout uri");
    let kotlin_uri = Url::from_file_path(&kotlin_path).expect("kotlin uri");
    indexer.index_layout_content(&layout_uri, FOO_BAR_LAYOUT);
    indexer.index_generated_bindings(&module_root);
    indexer.index_content(&kotlin_uri, kotlin_source);
    indexer.set_live_lines(&kotlin_uri, kotlin_source);
    indexer.store_live_tree(&kotlin_uri, kotlin_source);

    for (needle, qualifier) in [("binding.title", Some("binding")), ("it.title", None)] {
        let position = ViewBindingFixture::position_in(kotlin_source, needle);
        let context = if let Some(qualifier) = qualifier {
            ViewBindingFixture::cursor_context("title", Some(qualifier))
        } else {
            CursorContext::build(&indexer, &kotlin_uri, position).expect("cursor context")
        };
        let response = find_definition(&context, &*indexer, &kotlin_uri, position)
            .await
            .expect("definition for contextual receiver");
        let locations = response_locations(Some(response));
        assert_eq!(locations.len(), 1, "expected remap for {needle}");
        assert_eq!(uri_path_string(&locations[0]), "foo_bar.xml");
    }
}

#[test]
fn xml_definition_on_view_id_returns_all_variants() {
    let fixture = ViewBindingFixture::build();
    let layout_source =
        fs::read_to_string(fixture.land_layout_uri.to_file_path().unwrap()).unwrap();
    let position = ViewBindingFixture::position_in(&layout_source, "@+id/title");

    let response =
        find_layout_xml_definition(&*fixture.indexer, &fixture.land_layout_uri, position);
    let locations = response_locations(response);

    assert_eq!(locations.len(), 2);
    assert!(locations
        .iter()
        .any(|location| location.uri.as_str().contains("layout-land")));
}

#[test]
fn xml_implementation_on_fqn_tag_resolves_custom_class() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let layout_path = module_root.join("src/main/res/layout/custom.xml");
    fs::create_dir_all(layout_path.parent().unwrap()).expect("mkdir layout");
    let layout_source = r#"<?xml version="1.0" encoding="utf-8"?>
<com.example.widgets.CustomView xmlns:android="http://schemas.android.com/apk/res/android"
    android:layout_width="match_parent"
    android:layout_height="match_parent" />
"#;
    fs::write(&layout_path, layout_source).expect("layout");

    let custom_java = module_root.join("src/main/java/com/example/widgets/CustomView.java");
    fs::create_dir_all(custom_java.parent().unwrap()).expect("mkdir java");
    let custom_source = "package com.example.widgets;\npublic class CustomView {}\n";
    fs::write(&custom_java, custom_source).expect("custom java");

    let indexer = Indexer::new();
    let layout_uri = Url::from_file_path(&layout_path).expect("layout uri");
    let custom_uri = Url::from_file_path(&custom_java).expect("custom uri");
    indexer.index_layout_content(&layout_uri, layout_source);
    indexer.index_content(&custom_uri, custom_source);

    let position = ViewBindingFixture::position_in(layout_source, "com.example.widgets.CustomView");
    let response = find_layout_xml_implementation(&indexer, &layout_uri, position);
    let locations = response_locations(response);

    assert_eq!(locations.len(), 1);
    assert!(locations[0].uri.as_str().contains("CustomView.java"));
}

#[test]
fn xml_implementation_on_bare_tag_with_sdk_resolves_text_view() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let layout_path = module_root.join("src/main/res/layout/widget.xml");
    fs::create_dir_all(layout_path.parent().unwrap()).expect("mkdir layout");
    let layout_source = r#"<?xml version="1.0" encoding="utf-8"?>
<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
    android:layout_width="match_parent"
    android:layout_height="match_parent">

    <TextView
        android:layout_width="wrap_content"
        android:layout_height="wrap_content" />
</LinearLayout>
"#;
    fs::write(&layout_path, layout_source).expect("layout");

    let sdk_root = temp.path().join("android-sdk");
    let text_view_path = sdk_root.join("android/widget/TextView.java");
    fs::create_dir_all(text_view_path.parent().unwrap()).expect("mkdir sdk");
    let text_view_source = "package android.widget;\npublic class TextView {}\n";
    fs::write(&text_view_path, text_view_source).expect("textview");

    let indexer = Indexer::new();
    indexer.workspace_root.set(temp.path().to_path_buf());
    let layout_uri = Url::from_file_path(&layout_path).expect("layout uri");
    let text_view_uri = Url::from_file_path(&text_view_path).expect("textview uri");
    indexer.index_layout_content(&layout_uri, layout_source);
    indexer.index_content(&text_view_uri, text_view_source);

    let position = ViewBindingFixture::position_in(layout_source, "TextView");
    let response = find_layout_xml_implementation(&indexer, &layout_uri, position);
    let locations = response_locations(response);

    assert_eq!(locations.len(), 1);
    assert!(locations[0].uri.as_str().contains("TextView.java"));
}

#[test]
fn xml_implementation_on_bare_tag_without_sdk_is_empty() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let layout_path = module_root.join("src/main/res/layout/widget.xml");
    fs::create_dir_all(layout_path.parent().unwrap()).expect("mkdir layout");
    let layout_source = r#"<?xml version="1.0" encoding="utf-8"?>
<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
    android:layout_width="match_parent"
    android:layout_height="match_parent">

    <TextView
        android:layout_width="wrap_content"
        android:layout_height="wrap_content" />
</LinearLayout>
"#;
    fs::write(&layout_path, layout_source).expect("layout");

    let indexer = Indexer::new();
    let layout_uri = Url::from_file_path(&layout_path).expect("layout uri");
    indexer.index_layout_content(&layout_uri, layout_source);

    let position = ViewBindingFixture::position_in(layout_source, "TextView");
    let response = find_layout_xml_implementation(&indexer, &layout_uri, position);
    assert!(response.is_none());
}

#[test]
fn open_layout_routes_to_layout_side_index() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let layout_path = module_root.join("src/main/res/layout/foo_bar.xml");
    fs::create_dir_all(layout_path.parent().unwrap()).expect("mkdir layout");
    fs::write(&layout_path, FOO_BAR_LAYOUT).expect("layout");

    let indexer = Indexer::new();
    let layout_uri = Url::from_file_path(&layout_path).expect("layout uri");
    indexer.set_live_lines(&layout_uri, FOO_BAR_LAYOUT);
    indexer.index_layout_content(&layout_uri, FOO_BAR_LAYOUT);

    let data = indexer
        .layout_data_for_uri(layout_uri.as_str())
        .expect("layout indexed");
    assert_eq!(data.layout_name, "foo_bar");
    assert!(!data.view_ids.is_empty());
}

#[test]
fn binding_implementation_helper_filters_non_generated_classes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let handwritten = temp.path().join("FooBarBinding.java");
    fs::write(&handwritten, "public class FooBarBinding {}\n").expect("write");
    let uri = Url::from_file_path(&handwritten).expect("uri");
    let indexer = Indexer::new();
    indexer.index_content(&uri, "public class FooBarBinding {}\n");

    let context = ViewBindingFixture::cursor_context("FooBarBinding", None);
    let response = find_binding_implementation(&indexer, &context, &uri, Position::new(0, 7));
    assert!(response.is_none());
}
