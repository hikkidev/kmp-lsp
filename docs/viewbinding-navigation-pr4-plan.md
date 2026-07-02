# ViewBinding Navigation — PR 4 Implementation Plan

**Branch:** `feature/viewbinding-remap`  
**Base:** `feature/viewbinding-navigation` (includes merged PRs 1–3)  
**Normative design:** [`viewbinding-navigation.md`](./viewbinding-navigation.md) — § “PR 4 — Post-resolution remap: definition + implementation”

## Objective

Deliver the first **user-visible** ViewBinding navigation: remap Kotlin-side definition results from generated `*Binding.java` symbols to layout XML targets, keep implementation on binding types pointing at the raw generated Java class, and add XML-side definition/implementation for `@+id` attributes and view tags.

This PR is the navigation payoff. PRs 5 (hover + references) and 6 (diagnostics) build on it but are out of scope here.

## Dependencies on PRs 1–3

All satisfied on `feature/viewbinding-navigation`:

| Prior PR | PR 4 usage |
|---|---|
| PR 1 — `LayoutFileData`, `index_layout_content`, `layouts` side index | Remap targets: root tag, `@+id` positions, `<include>` tags across variants |
| PR 2 — `layouts_for_binding_class`, `is_generated_binding_uri`, `module_root_for_generated_file`, name mapping | Remap predicate, module pairing, class ↔ layout name |
| PR 3 — server-side databinding watcher | **Not required** for navigation correctness; only affects freshness after builds |

PR 4 does **not** require PR 5 or PR 6.

## Scope (in)

### 1. New module `src/features/viewbinding.rs`

Single choke point for Kotlin-side post-resolution remapping, plus XML-side navigation helpers.

#### 1a. Kotlin-side remap (definition only)

**`remap_generated_binding_definitions(index: &impl IndexRead, locations: Vec<Location>) -> Vec<Location>`**

For each input location:

1. Skip if `!index.is_generated_binding_uri(uri)`.
2. Derive `module_root` via `module_root_for_generated_file(path)`.
3. Identify the resolved symbol at `location.range` from `FileData.symbols` (match `selection_range` or enclosing symbol at range start).
4. Remap by symbol kind/name:

| Resolved symbol | Definition returns |
|---|---|
| Binding **class** (`FooBarBinding`, `SymbolKind::CLASS`) | Start of every layout variant from `layouts_for_binding_class(class_name, module_root)` — use `root_tag.range.start` as position (file start = range start of root tag per design: “start of the layout XML file(s)”; align with test fixtures — prefer `Position { line: 0, character: 0 }` if that matches existing LSP conventions for “file start”, else root tag start) |
| **Field** backed by `@+id` | `@+id` attribute range in every variant that declares the id (default first). Map field name → id via `binding_field_name_to_id` (camelCase → snake_case, inverse of AGP id→field rule) |
| **Field** backed by `<include>` | `<include>` tag range where `include.id` maps to the field name |
| **`rootView` field** or **`getRoot()` method** | XML root tag range (by field/method name, not id — covers AGP `bind()` special case) |

Non-binding locations pass through unchanged. Empty remap → return empty (silent miss).

**Name helper (new, in `binding_discovery.rs` or `viewbinding.rs`):**

- `binding_field_name_to_id(field_name: &str) -> String` — `fooBar` → `foo_bar` (Pascal/camel boundary split + lowercase; test round-trip with existing `snake_case_to_pascal_case` path for ids).

**Layout lookup helpers (new on `Indexer`, exposed via `IndexRead`):**

- `layout_data_for_uri(uri: &str) -> Option<Arc<LayoutFileData>>` — direct side-index read.
- `layouts_declaring_view_id(module_root: &Path, layout_name: &str, id: &str) -> Vec<(String /* uri */, Range)>` — scan `layouts` for matching module + layout name + id in `view_ids`; sort default variant first (reuse sort from `layouts_for_binding_class`).
- `include_tag_for_field(module_root, layout_name, field_name) -> Vec<(uri, Range)>` — match `includes` where `id` maps to field name.

#### 1b. Binding-type implementation (Kotlin)

**`find_binding_implementation(index, ctx, uri, position) -> Option<GotoDefinitionResponse>`**

Special case before the existing subtype BFS in `find_implementation`:

- When cursor is on a `*Binding` type name (import line or type usage) and `find_definition_qualified` resolves to a generated binding URI → return that Java class location **without remap**.
- Trigger heuristic: word ends with `"Binding"` **and** resolved location passes `is_generated_binding_uri`.
- Do **not** remap field accesses — the goal table only lists implementation for binding **types** and XML tags; field implementation stays on the existing path (likely empty, which is fine).

#### 1c. XML-side navigation

**`find_layout_xml_definition(index, uri, position) -> Option<GotoDefinitionResponse>`**

When cursor is inside a layout XML file (`is_layout_xml_path`):

- Detect `@+id/...` or `@id/...` in attribute value at cursor (tree-sitter-xml walk from `index.layout_data_for_uri` or live content).
- Return the id’s own declaration across all variants (self + siblings with same `layout_name` + `module_root`) — enables variant hopping.

**`find_layout_xml_implementation(index, uri, position) -> Option<GotoDefinitionResponse>`**

When cursor is on an element tag name:

- **Fully-qualified tag** (`com.example.CustomView`): `find_definition_qualified` with FQN.
- **Bare tag** (`TextView`): probe prefixes in order — `android.widget.`, `android.view.`, `android.webkit.` — against the index; first hit wins. Empty when SDK sources not indexed (no hardcoded map).

Register module in `src/features/mod.rs`.

### 2. Wire Kotlin-side definition remap

**`src/features/definition.rs`** — at end of `find_definition`, before returning:

```rust
let locs = /* existing resolution */;
let remapped = viewbinding::remap_generated_binding_definitions(index, locs);
locs_to_opt_response(remapped)
```

Requires `IndexRead` bound on the `index` parameter (already available through trait impls on `Indexer`).

**`src/backend/nav.rs`** — no change beyond what `find_definition` returns; jar rewrite runs after remap (XML targets are `file://`, not jar).

### 3. Wire Kotlin-side implementation

**`src/features/implementation.rs`** — at start of `find_implementation`:

1. Try `viewbinding::find_binding_implementation(...)` when word ends with `Binding`.
2. Fall through to existing method/type paths.

Generated binding **fields** resolved by the normal pipeline are **not** remapped for implementation (raw Java per design table).

### 4. Wire XML-side requests

**`src/backend/nav.rs`**

- `goto_definition_impl`: if `is_layout_xml_path`, try `find_layout_xml_definition` first; fall through to normal Kotlin/Java path.
- `goto_implementation_impl`: if `is_layout_xml_path`, try `find_layout_xml_implementation` first.

**`CursorContext::build`** — currently returns `None` when `word_and_qualifier_at` fails. For XML tag names and `@+id` values, ensure the cursor extractor works on layout files:

- Option A: extend `word_and_qualifier_at` / live-tree path for XML (if not already).
- Option B: dedicated XML cursor helper in `viewbinding.rs` that reads live lines + tree-sitter-xml (preferred — keeps XML logic colocated).

### 5. Minimal XML document lifecycle

Layout files are indexed during workspace scan and `didChangeWatchedFiles`, but **opened** layout files currently go through `index_content` (Kotlin/Java parser), not `index_layout_content`.

**`src/workspace/document_handler.rs`**

- In `spawn_open_document_indexing` / `handle_file_saved`: if `is_layout_xml_path`, call `index_layout_content` instead of `index_content`.
- In `store_live_document_state`: keep storing live lines (needed for cursor + unsaved edits).
- Live edit path (`file_change_handler`): route layout XML content changes to `index_layout_content`.

**`src/indexer/apply.rs`** — no scan changes (PR 1 already indexes layouts).

This ensures XML navigation works on files the user has open but that were not yet scanned, and keeps the side index fresh on edit.

### 6. Explicit non-goals (PR 4)

- Hover rendering (PR 5).
- References / receiver verification (PR 5).
- Diagnostics: build-required, viewBindingIgnore, staleness (PR 6).
- `R.id` resolution, intra-XML constraint references (follow-up TODOs in design doc).
- Remap on `goto_implementation` for binding fields (not in v1 goal table).

## Files to touch (expected)

| Area | Files |
|---|---|
| Remap + XML nav + tests | `src/features/viewbinding.rs`, `src/features/viewbinding_tests.rs` |
| Feature wiring | `src/features/mod.rs`, `src/features/definition.rs`, `src/features/implementation.rs` |
| Backend adapters | `src/backend/nav.rs` |
| Document lifecycle | `src/workspace/document_handler.rs`, `src/workspace/file_change_handler.rs` |
| Read helpers | `src/indexer/layout.rs` or `src/indexer/binding_discovery.rs`, `src/indexer/resolution.rs` |
| Integration test | `tests/lsp_smoke.rs` (new fixture + definition Kotlin→XML case) |

No new `Cargo.toml` dependencies. Reuse `tree-sitter-xml` from PR 1.

## Implementation approach (ordered)

1. **Pure helpers:** `binding_field_name_to_id`, layout lookup helpers on `Indexer` + `IndexRead`; unit tests with in-memory `LayoutFileData`.
2. **`remap_generated_binding_definitions`:** fixture-driven tests with pre-baked layout XML + fake `FooBarBinding.java` under `build/`; cover class, field, include, `rootView`, pass-through for non-binding URIs.
3. **Wire definition remap** — one call site in `find_definition`.
4. **Binding-type implementation** special case + tests.
5. **XML cursor + definition/implementation** — tree-sitter-xml position logic; SDK prefix probe tests with/without Android sources fixture.
6. **Document lifecycle** routing for open/save/change on layout paths.
7. **Binary smoke test** — end-to-end `textDocument/definition` from Kotlin import to layout XML.
8. **`cargo test` + `cargo clippy -- -D warnings`**.

## Test plan

Align with normative doc § PR 4 Tests. All tests use pre-baked fixtures under a fake module tree (`src/main/res/layout/…`, `build/generated/…/databinding/FooBarBinding.java`) — no Gradle build in CI.

| Test | Intent |
|---|---|
| Definition on import/type → layout(s) | `FooBarBinding` resolves to default + qualifier variants; default ordered first |
| Definition on `binding.field` → `@+id` | All declaring variants returned |
| Definition on `binding.header` → `<include>` tag | Include id field maps to tag range |
| Definition on `getRoot()` / `rootView` → root tag | AGP bind() special case |
| Implementation on binding type → raw Java | Returns generated `.java` class location, **not** layout |
| Competing definitions | Hand-written `FooBarBinding` in `src/` not remapped; module A layout ≠ module B generated class |
| Contextual receivers | `with(binding) { title }`, `binding.apply { title }`, `it.title` — definition remaps identically |
| Chained include | `binding.header.title` — field type `ViewHeaderBinding` remaps recursively through normal pipeline (integration-style) |
| XML implementation — bare tag | `<TextView>` with SDK sources indexed → `android.widget.TextView`; without SDK → empty |
| XML implementation — FQN tag | `<com.example.CustomView>` → custom class |
| XML definition on `@+id` | Returns declarations across variants |
| Document open routes to layout index | Open unsaved/new layout file → side index populated |
| Smoke (`tests/lsp_smoke.rs`) | JSON-RPC definition Kotlin→XML end-to-end |

Competing/misleading fixtures (project rule): hand-written binding class, wrong module pairing, identically named non-binding fields (for contextual receiver cases in PR 5 — stub here if needed for include chain only).

## Acceptance criteria

- `textDocument/definition` on `FooBarBinding` type/import → layout XML file start(s), default variant first.
- `textDocument/definition` on `binding.field` → `@+id` in every live variant that declares it.
- `textDocument/implementation` on binding type → generated Java class (unremapped).
- `textDocument/implementation` on XML view tag → view class (or empty without SDK).
- `textDocument/definition` on `@+id/field` in XML → id declarations across variants.
- Hand-written `*Binding` classes outside `build/` are never remapped.
- Layout files opened/edited in the editor stay indexed in the side index.
- All tests pass; clippy clean.

## Merge / stack notes

- Merge order: PR 1 → PR 2 → PR 3 → **PR 4** → PR 5 → PR 6.
- PR 5 depends on PR 4’s XML document handling and binding identification.
- PR 6 is independent of PR 4 code but sequenced after navigation is proven.

## Ambiguities / open decisions

1. **“Start of layout file” position** — design says “start of the layout XML file(s)”. Confirm whether LSP target is `(0, 0)` or root-tag start; match what existing navigation features use for file-level targets and lock in tests.
2. **`binding_field_name_to_id` edge cases** — acronyms (`XMLParser` → `x_m_l_parser` vs `xml_parser`)? AGP uses simple snake_case ids; mirror AGP’s camelCase-from-snake rule only (same as PR 2 name mapping inverse).
3. **XML cursor without live tree** — if layout file is indexed but not open, disk-backed content for tree-sitter walk vs requiring live lines; prefer reading from disk in XML helpers when live buffer absent.
4. **`find_implementation` on binding fields** — out of v1 goal table; current subtype BFS will return empty. No special case unless manual testing shows confusing behavior.
5. **Include field without `android:id`** — design says include **with** id is the declaration; include without id has no binding field — no remap, no test.
6. **`IndexRead` vs `SymbolIndex`** — remap needs `is_generated_binding_uri`, `layouts_for_binding_class`, `get_file_data`, and new layout helpers; extend `IndexRead` defaults rather than coupling to concrete `Indexer` in feature code.
