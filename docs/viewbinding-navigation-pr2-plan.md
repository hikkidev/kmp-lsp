# ViewBinding Navigation — PR 2 Implementation Plan

**Branch:** `feature/viewbinding-binding-discovery`  
**Base:** `feature/viewbinding-navigation` (includes merged PR 1 / `feature/viewbinding-layout-index`)  
**Normative design:** [`viewbinding-navigation.md`](./viewbinding-navigation.md) — § “PR 2 — Generated-binding discovery + module pairing”

## Objective

Discover AGP-generated `*Binding.java` files under each Android module’s `build/` tree, index them through the **existing Java pipeline** so symbols appear in `definitions` / `qualified`, and maintain a **binding side index** keyed by module root for query-time layout↔class pairing. No user-visible LSP navigation yet (PR 4+); PR 2 makes generated bindings resolvable and exposes read helpers for later PRs.

## Dependencies on PR 1

PR 1 must be present on the base branch (satisfied on `feature/viewbinding-navigation`):

| PR 1 deliverable | PR 2 usage |
|---|---|
| `layout_path_components` / `LayoutFileData.module_root` | Pair layouts with bindings in the same module root |
| `Indexer.layouts` + `index_layout_content` | Trigger additive discovery when a layout enters the side index |
| Layout watcher routing in `Backend::did_change_watched_files` | Same pattern extended for `build/**/databinding/*Binding.java` (best-effort) |
| Disk cache field `layouts` + `CACHE_VERSION` | Add parallel `generated_bindings` persistence; likely **second** `CACHE_VERSION` bump |

PR 2 does **not** require PR 3 (server-side poll watcher). PR 3 will call `watch_module` from `index_generated_bindings`; leave a clear extension point but do not implement the watcher in PR 2.

## Scope (in)

### 1. New module `src/indexer/binding_discovery.rs`

Core discovery and naming (pure functions + unit tests):

- **`GeneratedBindingEntry`**: `class_name`, `file_uri`, `modified_at` (`SystemTime`).
- **`ModuleBindings`**: per-module snapshot (e.g. `HashMap<String, GeneratedBindingEntry>` or `Vec` deduped by class name — pick one map keyed by class name for O(1) lookup).
- **`discover_generated_bindings(module_root: &Path) -> Vec<GeneratedBindingEntry>`**:
  - Walk `<module_root>/build/` recursively for `*Binding.java`.
  - **Do not** hardcode AGP output paths; any path under `build/` is allowed.
  - Read file (or first lines) to verify `package … .databinding` (package suffix **`.databinding`**, not merely containing the substring).
  - When debug/release (or other variants) produce duplicate class names, keep the entry with **latest mtime**.
  - Ignore files that fail package check or are unreadable (no panic).
- **`module_root_for_generated_file(path: &Path) -> Option<PathBuf>`**:
  - Mirror of PR 1 layout rule: path `<X>/build/.../FooBarBinding.java` → module root `<X>` (parent of the `build` directory segment).
- **Name mapping** (AGP conventions, shared by query helpers):
  - `binding_class_name_for_layout(layout_name: &str) -> String` — `foo_bar` → `FooBarBinding`.
  - `layout_name_for_binding_class(class_name: &str) -> Option<String>` — strip `Binding` suffix, PascalCase → snake_case; reject non-`*Binding` names.

Register module in `src/indexer.rs` (`mod binding_discovery;` + re-exports as needed).

### 2. Extend `Indexer` (`src/indexer.rs`)

- New field: `generated_bindings: DashMap<PathBuf, Arc<ModuleBindings>>` (module root → entries).
- **`index_generated_bindings(&self, module_root: &Path)`** (write path):
  1. Run `discover_generated_bindings`.
  2. Update `generated_bindings` for that module root.
  3. For each discovered file, read content and call existing **`index_content`** (Java parse) so symbols land in the normal index.
  4. Idempotent and additive — safe to call repeatedly; no full workspace rescan.
  5. **Extension point for PR 3:** document / stub hook where `DatabindingWatcherHandle::watch_module` will be invoked (not implemented in PR 2).

### 3. Query-time read helpers (pure `&self`, on `Indexer` or `impl Indexer` in `binding_discovery.rs`)

Per design doc:

- **`layouts_for_binding_class(&self, class_name: &str, module_root: &Path) -> Vec<Arc<LayoutFileData>>`**
  - Derive layout name from class name; scan `self.layouts` for matching `layout_name` + `module_root`.
  - Sort: default variant (`variant_qualifier == ""`) first, then others (stable ordering for qualifiers).
- **`is_generated_binding_uri(&self, uri: &str) -> bool`**
  - True iff URI is a discovered generated binding file for some module (side index membership), **not** merely “path contains build” or “class name ends with Binding”.
  - Used by PR 4 remap predicate.

Expose via `IndexRead` in `src/indexer/resolution.rs` if other features need these without reaching into `Indexer` internals (match existing patterns for layout reads).

### 4. Triggers (deferred / non-blocking)

**Rule:** no blocking work inside hot `apply` paths; follow `enrich.rs` / `index_source_paths` precedents (`tokio::spawn`, `spawn_blocking`, channel or mutex dedupe).

| Trigger | When | Action |
|---|---|---|
| Layout indexed | End of successful `index_layout_content` in `src/indexer/layout.rs` (after `layouts.insert`) | Enqueue `index_generated_bindings(module_root)` for `components.module_root` |
| Kotlin import | During Kotlin indexing in `src/indexer/apply.rs` (after `index_content` / file data available) | If any import path matches `*.databinding.*Binding` (suffix `Binding`, package segment `databinding`), derive importer’s module root and enqueue discovery for that module |
| Watched file (best-effort) | `Backend::did_change_watched_files` in `src/backend/mod.rs` | For paths matching `build/**/databinding/*Binding.java` (uri under module’s `build`, filename `*Binding.java`, parent path contains `databinding` segment), map to module root and enqueue re-discovery on create/change; on delete, remove from `generated_bindings` and drop Java index entries if existing deletion API allows (or re-run discovery for module) |

**Module root for Kotlin sources:** derive consistently with layout rule: `<X>/src/<sourceset>/...` → `<X>`. Reuse or extract shared helper next to `layout_path_components` to avoid drift.

**Deduping:** coalesce multiple enqueue calls for the same module while a job is in flight (mutex set or “last requested” flag).

### 5. Disk cache (`src/indexer/cache.rs`)

- Persist `generated_bindings` (module root string → serialized entries including uri + mtime + class name).
- `#[serde(default)]` on new cache fields.
- Bump **`CACHE_VERSION`** unless project policy folds PR 1+2 into one release without a bump (PR 1 already at 30 — **expect bump to 31** for PR 2).
- Round-trip test alongside layout cache tests.

### 6. Explicit non-goals (PR 2)

- Post-resolution remap (PR 4).
- XML document LSP routing (PR 4).
- Hover / references (PR 5).
- Diagnostics (PR 6).
- Server-side `build/` poll watcher (PR 3).
- Proactive full-workspace scan of all `build/` trees on every startup (only **additive** discovery when triggered).

## Files to touch (expected)

| Area | Files |
|---|---|
| Discovery + tests | `src/indexer/binding_discovery.rs`, `src/indexer/binding_discovery_tests.rs` (or `#[path]` module) |
| Indexer API | `src/indexer.rs` |
| Layout trigger | `src/indexer/layout.rs` |
| Kotlin trigger | `src/indexer/apply.rs` |
| Watcher routing | `src/backend/mod.rs` |
| Cache | `src/indexer/cache.rs`, `src/indexer/cache_tests.rs` or `binding_discovery` tests |
| Read trait | `src/indexer/resolution.rs` (+ `resolution_tests.rs` if helpers added) |
| Module wiring | `src/indexer/scan.rs` only if cache restore must hydrate `generated_bindings` on load (mirror `layouts` restore path) |

No new `Cargo.toml` dependencies.

## Implementation approach (ordered)

1. **Pure layer first:** naming helpers + `module_root_for_generated_file` + `discover_generated_bindings` with temp-dir fixture tests (nested fake AGP paths, package filter, mtime tie-break).
2. **`ModuleBindings` + Indexer field** and `index_generated_bindings` integrating `index_content` for each file.
3. **Read helpers** `layouts_for_binding_class`, `is_generated_binding_uri` with competing-definition fixtures.
4. **Background enqueue** helper (single place) used by layout + Kotlin triggers.
5. **Watcher branch** in `did_change_watched_files` before/after layout branch; share path-matching helper with discovery filter.
6. **Cache** serialize/deserialize + version bump + restore on workspace load if applicable.
7. **Clippy + full `cargo test`** per AGENTS.md.

## Test plan

Align with normative doc § PR 2 Tests:

| Test | Intent |
|---|---|
| Discovery temp module | Finds `*Binding.java` in varied `build/generated/...` paths; rejects wrong package; picks newer debug vs release copy |
| Name mapping | Round-trip `foo_bar` ↔ `FooBarBinding`; single-segment names; digits |
| Import-triggered indexing | Index Kotlin with `import com.app.databinding.FooBarBinding`; generated fixture under `build/` becomes resolvable via `qualified` / go-to-symbol |
| `is_generated_binding_uri` | True for discovered generated file; **false** for hand-written `FooBarBinding` in `src/main/java` |
| Watcher | Touch fixture `build/.../databinding/FooBarBinding.java`; side index + Java symbols update |
| Cache round-trip | `generated_bindings` survives save/load at new `CACHE_VERSION` |
| Layout trigger (recommended) | `index_layout_content` schedules discovery — assert via counter or mock channel in unit test |

Competing/misleading definitions in tests (project rule): same class name in non-generated sources, bindings in module A vs layout in module B.

## Acceptance criteria

- Generated binding Java under `build/` with valid `.databinding` package is indexed and resolvable like any Java class.
- `generated_bindings` reflects latest discovery per module; duplicate class names resolved by mtime.
- Layout or databinding import triggers discovery without blocking indexing.
- `layouts_for_binding_class` returns correct variants with default first.
- `is_generated_binding_uri` is precise enough for PR 4 remap gating.
- All tests pass; `cargo clippy -- -D warnings` clean.
- No LSP behavior change required yet (optional smoke: definition on binding still hits Java until PR 4).

## Merge / stack notes

- Merge order: PR 1 → **PR 2** → PR 3 → PR 4 → PR 5 → PR 6.
- PR 3 depends on PR 2’s `index_generated_bindings` write site for lazy `watch_module`.
- PRs 4–6 depend on PR 2 read helpers and generated symbol index.

## Ambiguities / open decisions

1. **`ModuleBindings` shape** — doc names the type but not fields; use class-name → entry map unless multiple URIs per class must be retained (design says one winner per class after mtime).
2. **`CACHE_VERSION`** — doc says bump “if not already covered by PR 1”; PR 1 already bumped; PR 2 should bump again when adding cache fields.
3. **Kotlin module-root derivation** — doc assumes parity with layout paths; if KMP places sources outside `src/<sourceset>/`, define explicit rules or reuse existing module-root logic elsewhere in the indexer.
4. **Import pattern** — clarify whether `import …Binding` without `.databinding` segment should trigger (design says `*.databinding.*Binding`; stick to that).
5. **Delete handling** — when a generated file disappears, whether to prune Java `definitions` immediately or rely on re-discovery empty snapshot; match existing `FileDeleted` / re-index patterns.
6. **Branch naming** — design doc does not specify; this repo used `feature/viewbinding-layout-index` for PR 1; PR 2 uses `feature/viewbinding-binding-discovery`.
