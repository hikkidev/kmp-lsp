use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::backend::databinding_watcher::spawn_databinding_watcher_with_interval;
use crate::indexer::{DatabindingWatcherHandle, Indexer};
use crate::workspace::Event;

const SAMPLE_BINDING_JAVA: &str = r#"package com.example.app.databinding;

import android.view.LayoutInflater;
import android.view.View;

public final class FooBarBinding {
    private FooBarBinding() {}
}
"#;

const WRONG_PACKAGE_BINDING_JAVA: &str = r#"package com.example.app.ui;

public final class FooBarBinding {
}
"#;

fn write_binding_java(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir binding dir");
    }
    fs::write(path, content).expect("write binding java");
}

fn binding_path(module_root: &Path) -> PathBuf {
    module_root.join(
        "build/generated/data_binding_base_class_source_out/debug/out/com/example/app/databinding/FooBarBinding.java",
    )
}

async fn poll_until<F: Fn() -> bool>(condition: F, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            if condition() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition not met within timeout");
}

fn spawn_test_watcher(
    indexer: Arc<Indexer>,
    republish_tx: mpsc::Sender<Event>,
) -> DatabindingWatcherHandle {
    spawn_databinding_watcher_with_interval(indexer, republish_tx, Duration::from_millis(50))
}

#[tokio::test]
async fn watch_module_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let binding_path = binding_path(&module_root);
    write_binding_java(&binding_path, SAMPLE_BINDING_JAVA);

    let indexer = Arc::new(Indexer::new());
    let (republish_tx, _republish_rx) = mpsc::channel(4);
    let handle = spawn_test_watcher(Arc::clone(&indexer), republish_tx);

    indexer.set_databinding_watcher_handle(handle.clone());
    indexer.index_generated_bindings(&module_root);
    let parse_count_after_first = indexer.parse_count.load(Ordering::Relaxed);

    indexer.index_generated_bindings(&module_root);
    let parse_count_after_second = indexer.parse_count.load(Ordering::Relaxed);

    assert_eq!(
        parse_count_after_first, parse_count_after_second,
        "duplicate watch_module registration must not trigger extra discovery"
    );
}

#[tokio::test]
async fn watcher_indexes_new_binding_file_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let binding_path = binding_path(&module_root);

    let indexer = Arc::new(Indexer::new());
    let (republish_tx, _republish_rx) = mpsc::channel(4);
    let handle = spawn_test_watcher(Arc::clone(&indexer), republish_tx);
    indexer.set_databinding_watcher_handle(handle.clone());
    handle.watch_module(&module_root);

    write_binding_java(&binding_path, SAMPLE_BINDING_JAVA);

    let qualified_key = "com.example.app.databinding.FooBarBinding";
    let indexer = Arc::clone(&indexer);
    poll_until(
        move || indexer.qualified.contains_key(qualified_key),
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn watcher_ignores_non_binding_and_outside_databinding_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    fs::create_dir_all(module_root.join("build/tmp")).expect("mkdir tmp");

    let indexer = Arc::new(Indexer::new());
    let (republish_tx, _republish_rx) = mpsc::channel(4);
    let handle = spawn_test_watcher(Arc::clone(&indexer), republish_tx);
    indexer.set_databinding_watcher_handle(handle.clone());
    handle.watch_module(&module_root);

    let parse_count_before = indexer.parse_count.load(Ordering::Relaxed);

    fs::write(module_root.join("build/tmp/whatever.txt"), "not a binding").expect("write txt");
    write_binding_java(
        &module_root.join("build/generated/source/FooBarBinding.java"),
        WRONG_PACKAGE_BINDING_JAVA,
    );

    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        indexer.parse_count.load(Ordering::Relaxed),
        parse_count_before,
        "non-binding files must not trigger re-discovery"
    );
    assert!(!indexer
        .qualified
        .contains_key("com.example.app.databinding.FooBarBinding"));
}

#[tokio::test]
async fn watcher_triggers_when_build_dir_appears_after_registration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    fs::create_dir_all(&module_root).expect("mkdir module");

    let indexer = Arc::new(Indexer::new());
    let (republish_tx, _republish_rx) = mpsc::channel(4);
    let handle = spawn_test_watcher(Arc::clone(&indexer), republish_tx);
    indexer.set_databinding_watcher_handle(handle.clone());
    handle.watch_module(&module_root);

    write_binding_java(&binding_path(&module_root), SAMPLE_BINDING_JAVA);

    let qualified_key = "com.example.app.databinding.FooBarBinding";
    let indexer = Arc::clone(&indexer);
    poll_until(
        move || indexer.qualified.contains_key(qualified_key),
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn rapid_binding_writes_coalesce_to_one_rediscovery() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_root = temp.path().join("app");
    let binding_path = binding_path(&module_root);
    write_binding_java(&binding_path, SAMPLE_BINDING_JAVA);

    let indexer = Arc::new(Indexer::new());
    let (republish_tx, _republish_rx) = mpsc::channel(4);
    let handle = spawn_test_watcher(Arc::clone(&indexer), republish_tx);
    indexer.set_databinding_watcher_handle(handle.clone());
    indexer.index_generated_bindings(&module_root);

    tokio::time::sleep(Duration::from_millis(120)).await;
    let parse_count_after_initial = indexer.parse_count.load(Ordering::Relaxed);
    assert!(parse_count_after_initial >= 1);

    // Advance to a new whole-second mtime bucket on coarse filesystems.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    for suffix in 1..=5 {
        let content = format!("{SAMPLE_BINDING_JAVA}\n// touch {suffix}\n");
        write_binding_java(&binding_path, &content);
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let parse_count_after_burst = indexer.parse_count.load(Ordering::Relaxed);
    assert_eq!(
        parse_count_after_burst,
        parse_count_after_initial + 1,
        "rapid writes within one poll interval should coalesce to a single re-discovery"
    );
}
