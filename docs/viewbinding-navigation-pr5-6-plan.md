# ViewBinding Navigation — PR 5 & PR 6 Implementation Plan

**Branch:** `feature/viewbinding-hover-references` (PR 5), then `feature/viewbinding-diagnostics` (PR 6)  
**Base:** `feature/viewbinding-navigation` (includes merged PRs 1–4)  
**Normative design:** [`viewbinding-navigation.md`](./viewbinding-navigation.md) — § “PR 5 — Hover nullability + receiver-verified references” and § “PR 6 — Diagnostics”

---

## PR 5 — Hover nullability + receiver-verified references

### Objective

Complete the remaining **navigation** features: Kotlin-style hover on binding fields (`val title: TextView` / `val title: TextView?`), and receiver-verified `textDocument/references` for binding fields from both Kotlin and XML entry points. Generated binding URIs stay excluded from reference results.

PR 6 (diagnostics) is out of scope here.

### Dependencies on PRs 1–4

All satisfied on `feature/viewbinding-navigation`:

| Prior PR | PR 5 usage |
|---|---|
| PR 1 — `LayoutFileData`, `view_ids`, `includes`, `view_binding_ignore` | XML-side references entry: id → field name, layout/module context |
| PR 2 — `is_generated_binding_uri`, name mapping, `generated_bindings`, `layouts_for_binding_class` | Identify binding fields, pair layout ↔ class for verification |
| PR 3 — databinding watcher + `RepublishOpenFileDiagnostics` | Not required for hover/references correctness |
| PR 4 — `viewbinding.rs`, XML document lifecycle, `binding_field_name_to_id`, layout lookup helpers | Binding identification, XML cursor helpers, fixture patterns |

PR 5 does **not** require PR 6.

### Scope (in)

#### 1. `@Nullable` extraction at Java parse time

Extend `push_field_declaration` in `src/parser.rs` to detect `@Nullable` (and `@androidx.annotation.Nullable` / `@org.jetbrains.annotations.Nullable` if present in fixture sources) on the field declaration node or the lines immediately above it — mirror the existing `deprecated_at_line` upward-scan pattern.

Store on `SymbolEntry`:

```rust
/// True when the Java field carries `@Nullable` (AGP emits this for ids absent in some variants).
#[serde(default)]
pub nullable: bool,
```

**Decision:** parse-time flag on `SymbolEntry` (same pattern as `deprecated`), not lazy file reads in the hover path. AGP binding files are small; one bool per field is negligible. Avoids re-reading `build/` on every hover. **No `CACHE_VERSION` bump** unless serde layout changes require it — `#[serde(default)]` handles older cache entries.

#### 2. Binding-field hover rendering

**New helpers in `src/features/viewbinding.rs` (or `src/backend/format.rs` if presentation-only):**

- **`binding_id_to_field_name(id: &str) -> String`** — inverse of `binding_field_name_to_id`: `foo_bar` → `fooBar` (snake_case → camelCase; add to `binding_discovery.rs` alongside existing name mapping).
- **`short_type_name(type_name: &str) -> String`** — strip package prefix; keep simple generic args if needed (`TextView`, `ViewHeaderBinding`, not FQN).
- **`format_binding_field_hover(field_name: &str, type_name: &str, nullable: bool) -> String`** — produces `val title: TextView` or `val title: TextView?`; use `format_contextual_hover`-style fenced Kotlin block via existing `format.rs` helpers.

**Type extraction:** parse the Java field type from `SymbolEntry.detail` (e.g. `"TextView title"` / `"public final TextView title"`) or from the `field_declaration` CST during parse if detail is unreliable — prefer detail first, fall back to CST.

**Wire into hover pipeline (`src/features/hover.rs`):**

After `resolve_symbol_info` / `enrich_at_location` succeeds (both `contextual_receiver_hover` and `regular_symbol_hover` paths):

1. If resolved `location.uri` passes `index.is_generated_binding_uri(...)` **and** symbol is `FIELD`/`PROPERTY`/`VARIABLE` → return `format_binding_field_hover(...)` instead of `format_symbol_hover`.
2. Include-typed fields (`ViewHeaderBinding header`) naturally render as `val header: ViewHeaderBinding` — no special case.

Do **not** alter hover for binding **class** symbols (type/import hover stays on existing Java class signature).

#### 3. Receiver-verified references (Kotlin entry)

**New in `src/features/viewbinding.rs`:**

```rust
pub(crate) async fn find_binding_field_references(
    index: &(impl SymbolIndex + DocumentAccess + ScopeQuery + SearchAccess + IndexRead + Send + Sync),
    expected_binding_class: &str,
    field_name: &str,
    uri: &Url,
    line: u32,
    include_decl: bool,
) -> Vec<Location>
```

**Algorithm (two-stage, per design):**

1. **Candidate gathering** — delegate to existing `find_references_with_qualifier(field_name, None, uri, line, include_decl, index)` (or internal `ReferenceSearch` + `rg_locations` if qualifier scoping would over-narrow). This reuses rg, import-graph narrowing, and live-buffer injection unchanged.

2. **Receiver verification** — for each candidate location:
   - Parse the access site receiver (navigation expression before `.field_name`) from live/disk lines + tree-sitter Kotlin.
   - Infer receiver type via existing `infer_receiver_type` / `Resolver::resolve_member` / variable-type inference (`src/resolver/infer/`, `src/resolver/resolve.rs`).
   - **Keep** only when receiver type's leaf or qualified name resolves to `expected_binding_class` (e.g. `FooBarBinding` or `com.example.app.databinding.FooBarBinding`).
   - **Drop** unresolvable receivers (false-negative bias per design).
   - **Drop** locations in generated binding URIs (`is_generated_binding_uri`).

**Expected binding class resolution (request site):**

| Request origin | How to get `expected_binding_class` |
|---|---|
| Kotlin `binding.field` | Infer type of receiver (`binding`) at cursor; leaf must end with `Binding` and resolve to a generated class in the index |
| Kotlin generated Java field decl | Class name from enclosing `*Binding` class in same file |
| XML `@+id/field` | `binding_class_name_for_layout(layout_name)` from `LayoutFileData` at cursor |

**New helper:** `resolve_expected_binding_class(index, uri, position, ctx) -> Option<String>` — encapsulates the three cases above.

#### 4. XML-side references entry

**`find_layout_xml_references(index, uri, position, include_decl) -> Option<Vec<Location>>`**

When cursor is in a layout XML file:

1. Detect `@+id/...` at cursor (reuse XML cursor walk from PR 4's `find_layout_xml_definition`).
2. Read `LayoutFileData` for uri → `module_root`, `layout_name`, id string.
3. Map id → field name via `binding_id_to_field_name`.
4. `expected_class = binding_class_name_for_layout(layout_name)`.
5. Call `find_binding_field_references(...)` with a synthetic decl-site uri/line (the id attribute position) for scope purposes.

**Wire in `src/backend/handlers.rs` — `references_impl`:**

Before `CursorContext::build` (or after, with XML-specific early exit):

```rust
if is_layout_xml_path(&path) {
    if let Some(locs) = viewbinding::find_layout_xml_references(...).await {
        return Ok((!locs.is_empty()).then_some(locs));
    }
}
```

Kotlin-side: when `resolve_expected_binding_class` returns `Some(class)` for a lowercase field name with qualifier (receiver), route to `find_binding_field_references` instead of plain `find_references_with_qualifier`. Detection heuristic: cursor word is lowercase **and** (qualifier present **or** request from generated binding field decl) **and** expected binding class resolves.

#### 5. Explicit non-goals (PR 5)

- Diagnostics (PR 6).
- `R.id` references (follow-up TODO).
- References on binding **type** name (`FooBarBinding`) — not in v1 goal table.
- Broadening verification to non-binding classes with coincidentally named fields.

### Files to touch (expected)

| Area | Files |
|---|---|
| Nullable parse + SymbolEntry | `src/types.rs`, `src/parser.rs`, `src/parser_tests.rs` |
| Name mapping inverse | `src/indexer/binding_discovery.rs`, `binding_discovery_tests.rs` |
| Hover + references + tests | `src/features/viewbinding.rs`, `src/features/viewbinding_tests.rs` |
| Hover wiring | `src/features/hover.rs`, optionally `src/backend/format.rs` |
| References wiring | `src/features/references.rs` (thin dispatch), `src/backend/handlers.rs` |
| Read helper | `src/indexer/resolution.rs` (`IndexRead` if new query needed) |
| Feature module | `src/features/mod.rs` |

No new `Cargo.toml` dependencies.

### Implementation approach (ordered)

1. **`binding_id_to_field_name`** + round-trip tests with `binding_field_name_to_id`.
2. **`SymbolEntry.nullable`** + Java `@Nullable` detection + parser tests.
3. **`format_binding_field_hover`** + `short_type_name` + unit tests.
4. **Wire hover** — binding-field branch in `compute_hover` paths; fixture tests with `@Nullable` and include-typed fields.
5. **`find_binding_field_references`** + receiver verification helper (extract receiver from nav expr at candidate site).
6. **`resolve_expected_binding_class`** + Kotlin-side dispatch in `references_impl` / thin wrapper in `references.rs`.
7. **`find_layout_xml_references`** + XML routing in `handlers.rs`.
8. **`cargo test` + `cargo clippy -- -D warnings`**.

### Test plan (PR 5)

Reuse PR 4 fixture module tree (`ViewBindingFixture` in `viewbinding_tests.rs`); extend Java fixtures with `@Nullable`:

```java
import androidx.annotation.Nullable;
public final TextView subtitle; // or @Nullable on preceding line
```

| Test | Intent |
|---|---|
| Hover — non-nullable field | `binding.title` → `val title: TextView`; no FQN |
| Hover — `@Nullable` field | `binding.subtitle` → `val subtitle: TextView?` |
| Hover — include field | `binding.header` → `val header: ViewHeaderBinding` |
| Hover — binding class unchanged | Hover on `FooBarBinding` type still Java-style class hover |
| References — qualified | `binding.title` usages across files found |
| References — contextual | `with(binding) { title }`, `binding.apply { title }`, `it.title` found |
| References — misleading competitor | Another class with `title` property **excluded** |
| References — unresolvable receiver | `unknown.title` dropped, not included |
| References — generated Java excluded | No hits inside `build/.../FooBarBinding.java` |
| XML → references | `@+id/title` in layout returns same set as Kotlin-side on `binding.title` |
| XML vs Kotlin parity | Assert equal location sets (modulo ordering) |

Competing fixtures: hand-written class `title` property in same package; second `FooBarBinding`-like class in another module with `title` field.

### Acceptance criteria (PR 5)

- Hover on `binding.field` where field resolves to generated binding Java → Kotlin-style `val field: Type` / `val field: Type?`.
- Short type names only (no `android.widget.TextView` in hover).
- References on binding fields return all verified Kotlin usages; false-positive competitors excluded.
- References from XML `@+id` match Kotlin-side results.
- Generated binding files never appear in reference results.
- All tests pass; clippy clean.

### Open decisions (PR 5)

1. **Kotlin-side references dispatch** — intercept in `references_impl` only when `resolve_expected_binding_class` succeeds (safest), vs always post-filtering `find_references_with_qualifier` output for lowercase field names (simpler but slower). Prefer **intercept when binding context is confirmed**.
2. **`binding_id_to_field_name` edge cases** — mirror AGP only (`foo_bar` → `fooBar`); no acronym special-casing unless tests prove AGP differs.
3. **Receiver verification for scope-function blocks** — reuse `infer_receiver_type` with `ReceiverKind::Contextual`; PR 4 contextual-receiver definition tests are the template.
4. **Include decl without id** — no binding field → no references entry point; no test.

---

## PR 6 — Diagnostics: build-required, viewBindingIgnore, staleness

### Objective

Emit the three ViewBinding diagnostics on the **Kotlin side only**: build-required warning on databinding imports, opt-out warning for `tools:viewBindingIgnore`, and Information-level staleness on usages of fields that exist in generated Java but whose id is gone from all live layout variants. Self-clearing after build relies on PR 3 infrastructure — no new watcher code.

Depends on PRs 1–3 for index data and self-clearing; **independent of PR 5** at the code level but sequenced after PR 5 in the stack.

### Dependencies

| Prior PR | PR 6 usage |
|---|---|
| PR 1 — `layouts`, `view_binding_ignore`, `view_ids` | Import pairing, opt-out detection, staleness id lookup |
| PR 2 — `generated_bindings`, `import_triggers_binding_discovery`, name mapping | “Generated class exists?” check per import |
| PR 3 — watcher → `RepublishOpenFileDiagnostics` | Build-required diagnostic self-clears after build |
| PR 4–5 | Not required for diagnostic logic |

### Scope (in)

#### 1. New module `src/features/viewbinding_diagnostics.rs` (+ `viewbinding_diagnostics_tests.rs`)

**`viewbinding_import_diagnostics(index: &Indexer, uri: &Url) -> Vec<Diagnostic>`**

For each import in `FileData.imports` where `import_triggers_binding_discovery(&import.full_path)`:

1. Extract binding class name from import leaf (e.g. `com.example.app.databinding.FooBarBinding` → `FooBarBinding`).
2. Derive `module_root` from `uri` via `module_root_for_source_file`.
3. Look up `layout_name = layout_name_for_binding_class(class_name)`.
4. **No layout in index for `(module_root, layout_name)`** → emit nothing (plain unresolved import; existing behavior).
5. **Layout exists, any variant has `view_binding_ignore == true`** → **Warning** on import range: distinct opt-out message (`tools:viewBindingIgnore`). Takes precedence over build-required.
6. **Layout exists, no generated entry** in `generated_bindings[module_root].entries[class_name]` → **Warning**: “ViewBinding class not generated — build the project”.
7. **Generated class present** → no diagnostic.

**New read helper on `Indexer` / `IndexRead`:**

```rust
fn generated_binding_discovered(&self, module_root: &Path, class_name: &str) -> bool
fn layout_exists_for_binding(&self, module_root: &Path, layout_name: &str) -> bool
fn any_layout_variant_ignores_view_binding(&self, module_root: &Path, layout_name: &str) -> bool
```

Pure `&self` reads over `layouts` + `generated_bindings`.

**`stale_binding_field_diagnostics(index: &Indexer, uri: &Url, document: &LiveDoc) -> Vec<Diagnostic>`**

Walk Kotlin `navigation_expression` nodes (same CST walk pattern as `nullable_dot_call_diagnostics`):

For each `.field` access where:

1. Receiver type resolves to a generated `*Binding` class (`expected_binding_class` helper — can share with PR 5 or duplicate thin wrapper).
2. Field exists on generated Java class (symbol in binding file).
3. Mapped id (`binding_field_name_to_id(field)`) is **absent** from `layouts_declaring_view_id` for **every** variant of that layout in the module.

→ emit **Information** diagnostic on the field identifier range: “field comes from a stale build; id no longer in layout”.

**Skip when:**

- Id present in at least one live variant (partial variant coverage is normal — `@Nullable` fields).
- Receiver or field unresolvable (silent skip, consistent with navigation).
- File is not Kotlin (XML stays diagnostic-free per design).

#### 2. Wire into diagnostic publication

**`src/workspace/document_handler.rs`** and **`src/workspace/file_change_handler.rs`** — inside existing `spawn_blocking` blocks, after `nullable_dot_call_diagnostics`, gated on `!indexing_in_progress`:

```rust
diagnostics.extend(viewbinding_import_diagnostics(&indexer, &uri));
if let Some(ref document) = live_doc {
    diagnostics.extend(stale_binding_field_diagnostics(&indexer, &uri, document));
}
```

Apply in **both** initial publish paths and `republish_open_file_diagnostics`.

Import diagnostics do not require `LiveDoc`; staleness does.

#### 3. Self-clearing (no new code)

PR 3 watcher already sends `Event::RepublishOpenFileDiagnostics` after `index_generated_bindings`. PR 6 tests assert the build-required warning disappears after fixture binding file appears + republish.

#### 4. Explicit non-goals (PR 6)

- XML-file diagnostics.
- Diagnostics on unresolved binding imports with no layout.
- Blocking navigation requests with diagnostic messages (requests stay silently empty).

### Files to touch (expected)

| Area | Files |
|---|---|
| Diagnostics + tests | `src/features/viewbinding_diagnostics.rs`, `viewbinding_diagnostics_tests.rs` |
| Read helpers | `src/indexer/binding_discovery.rs` or `resolution.rs` |
| Wiring | `src/workspace/document_handler.rs`, `file_change_handler.rs` |
| Feature module | `src/features/mod.rs` |

No new dependencies. No `CACHE_VERSION` bump.

### Implementation approach (ordered)

1. **Read helpers** — `generated_binding_discovered`, `layout_exists_for_binding`, `any_layout_variant_ignores_view_binding`; unit tests with in-memory index state.
2. **`viewbinding_import_diagnostics`** — import scan + three outcome branches; tests.
3. **`stale_binding_field_diagnostics`** — CST walk + binding field check; tests with id removed from all layout fixtures.
4. **Wire both** into document_handler + file_change_handler + republish path.
5. **Self-clear integration test** — start without generated Java (warning present), write binding file, trigger watcher/republish (reuse `databinding_watcher_tests` / `poll_until` pattern), assert diagnostic gone.
6. **`cargo test` + `cargo clippy -- -D warnings`**.

### Test plan (PR 6)

| Test | Intent |
|---|---|
| Import — layout + no generated | Warning on import line (build-required) |
| Import — `viewBindingIgnore` | Opt-out warning; **not** build-required |
| Import — generated present | No diagnostic |
| Import — no layout | No new diagnostic (non-existent binding) |
| Staleness — id removed everywhere | Info on `binding.oldField` usage |
| Staleness — id in one variant | No diagnostic |
| Staleness — XML file | No diagnostics emitted |
| Self-clear | No generated → warning; add `FooBarBinding.java` + republish → warning gone |

Fixtures: extend PR 4 module tree; staleness test uses generated Java with `oldField` but layout XML without matching `@+id/old_field`.

### Acceptance criteria (PR 6)

- Build-required warning on databinding import when layout exists but generated class not discovered.
- Distinct opt-out warning when any layout variant has `tools:viewBindingIgnore="true"`.
- Staleness Info on Kotlin usages of fields missing from all live layout variants.
- No diagnostics in XML files.
- Build-required self-clears after generated binding indexed + diagnostics republished.
- All tests pass; clippy clean.

### Open decisions (PR 6)

1. **Import diagnostic range** — span the full import line (match `missing_package_diagnostic` style) vs import symbol range from `ImportEntry`.
2. **`viewBindingIgnore` on qualifier variant only** — if default variant is normal but `layout-land` ignores, still warn on import (design: “layout opts out”).
3. **Staleness for include fields** — check include id in `includes` list, not only `view_ids`; include removed → stale if field still in generated Java.
4. **PR 5 code sharing** — if PR 5 lands `resolve_expected_binding_class`, reuse in staleness walk; else duplicate minimal receiver→binding-class resolution to keep PR 6 mergeable independently (prefer reuse if same branch stack).

---

## Stack / merge notes

```
PR 1 → PR 2 → PR 3 → PR 4 → PR 5 → PR 6
```

- **PR 5 branch:** `feature/viewbinding-hover-references`, base `feature/viewbinding-navigation` (or `main` after PR 4 merge).
- **PR 6 branch:** `feature/viewbinding-diagnostics`, base after PR 5 merge (or parallel base `feature/viewbinding-navigation` if stacking — PR 6 does not depend on PR 5 code, only stack order).
- After each merge: `cargo build && cargo test` on `main`, rebase dependent branch.
- PR 6 self-clear test may reuse `spawn_databinding_watcher_with_interval` + `poll_until` from `databinding_watcher_tests.rs`.

## Combined acceptance (PRs 5 + 6 complete)

All rows from the normative design goal table are covered:

| LSP request | Status after PR 4 | After PR 5 | After PR 6 |
|---|---|---|---|
| definition — binding type/field | ✓ PR 4 | ✓ | ✓ |
| implementation — binding type, XML tags | ✓ PR 4 | ✓ | ✓ |
| references — binding field | — | ✓ PR 5 | ✓ |
| references — `@+id` (XML) | — | ✓ PR 5 | ✓ |
| hover — binding field | — | ✓ PR 5 | ✓ |
| diagnostics — import / staleness | — | — | ✓ PR 6 |
