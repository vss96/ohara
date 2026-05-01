# ohara v0.4 — Java + Kotlin support implementation plan

> **For agentic workers:** Use TDD red/green commits per task. Standards
> match Plan 1–3 (no commit attribution; workspace-green at every
> commit; cargo fmt + clippy + test clean at end).

**Goal:** index Java 17/21+ and Kotlin 1.9/2.0+ source — sealed types,
records, data classes, objects, annotations on declarations — so
`find_pattern` returns useful results on JVM codebases (Spring-flavored
codebases especially).

**Architecture:** v0.4 is a pure parse-layer addition. Two new modules
under `crates/ohara-parse/src/` (`java.rs`, `kotlin.rs`) mirror the
shape of the existing `rust.rs`/`python.rs`. The language-agnostic AST
chunker (Plan 3) consumes their output unchanged. No storage, retriever,
MCP, or migration changes.

**Tech Stack:** Rust 2021, tree-sitter 0.22, tree-sitter-java
(latest 0.21+), tree-sitter-kotlin (canonical crate or actively
maintained fork). All else inherited from Plan 3.

---

## 0. Findings to verify before writing code

- [x] **Pick `tree-sitter-java` version: `0.21.0`.** Latest 0.21.x on
      crates.io (newer 0.23.x require `tree-sitter >=0.23` and break our
      0.22 pin). 0.21.0 declares `tree-sitter >=0.21.0`, compatible with
      our pin. Confirmed `node-types.json` exposes
      `class_declaration`, `interface_declaration`, `record_declaration`,
      `enum_declaration`, `annotation_type_declaration`,
      `method_declaration`, `constructor_declaration`, `marker_annotation`,
      and `annotation`. `sealed` shows up as a modifier child of the
      ordinary `class_declaration` / `interface_declaration` — there is
      no distinct `sealed_class_declaration` AST type, so we capture all
      sealed forms via the standard declaration patterns.
- [x] **Pick `tree-sitter-kotlin` version: `0.3.8`** (canonical
      `fwcd/tree-sitter-kotlin`). Declares `tree-sitter >=0.21, <0.23`,
      compatible with our 0.22 pin. Confirmed `node-types.json` exposes
      `class_declaration`, `object_declaration`, `companion_object`,
      `function_declaration`, and `annotation`. Kotlin's grammar treats
      interface as a flavor of `class_declaration` (carrying an
      `interface` keyword child rather than a distinct AST node), and
      `sealed`/`data` are modifiers on `class_declaration`. The newer
      `tree-sitter-kotlin-ng` 1.x crate exists but pins
      `tree-sitter ^0.25` so it cannot be used here.

## 1. Interface contracts

No new public types. Both new modules expose:

```rust
pub fn extract(text: &str, path: &str) -> Vec<Symbol>;
```

returning the **per-file flat list of top-level + nested symbols in
source byte order** (the chunker contract). `Symbol::sibling_names`
stays `Vec::new()` — the AST chunker (Plan 3) populates it later when
merging chunks.

`Symbol::span_start` / `span_end` cover the declaration **including**
preceding annotations and modifiers. This is the only behavioral
deviation from `rust.rs` / `python.rs`, where attributes/decorators are
NOT yet annotation-spanned. Future re-extraction of Rust/Python to
match is out of scope.

## 2. File ownership

Single-track plan; no parallel split needed (small scope, ~2 days).

| File | Status | Owner |
|------|--------|-------|
| `Cargo.toml` (workspace) | edit (add 2 deps) | this plan |
| `crates/ohara-parse/Cargo.toml` | edit | this plan |
| `crates/ohara-parse/src/lib.rs` | edit (add modules + dispatch) | this plan |
| `crates/ohara-parse/src/java.rs` | new | this plan |
| `crates/ohara-parse/src/kotlin.rs` | new | this plan |

No other files change.

## 3. Ordered tasks (TDD red/green)

### Task 1: Add tree-sitter-java workspace dep

- [ ] **1.r:** No test (manifest-only). Single commit: edit
      `Cargo.toml` (workspace), add `tree-sitter-java = "<verified
      version>"`. Verify `cargo build -p ohara-parse` still passes
      (the dep is declared but not consumed yet).

### Task 2: Java extractor — class + interface + sealed

- [ ] **2.r:** Write `extracts_simple_class` and
      `extracts_sealed_interface` tests in `java.rs::tests`.
      Stub `pub fn extract(...) -> Vec<Symbol> { vec![] }`. Tests fail.
- [ ] **2.g:** Implement using a tree-sitter query that captures
      `class_declaration`, `interface_declaration`, and their sealed
      variants. Map to `SymbolKind::Class`. Tests pass.

### Task 3: Java extractor — methods + constructors

- [ ] **3.r:** `extracts_methods_inside_class` and
      `constructor_kind_is_method`. Tests fail.
- [ ] **3.g:** Extend the query to capture `method_declaration` and
      `constructor_declaration`. `SymbolKind::Method`. The
      constructor's `name` is the enclosing class name. Tests pass.

### Task 4: Java extractor — records + enums + annotation types

- [ ] **4.r:** `extracts_record_as_class`, `extracts_enum`,
      `extracts_annotation_type`. Tests fail.
- [ ] **4.g:** Extend the query to capture `record_declaration`,
      `enum_declaration`, `annotation_type_declaration`. All map to
      `SymbolKind::Class`. Tests pass.

### Task 5: Java extractor — annotations preserved in source_text

- [ ] **5.r:** `preserves_annotations_in_source_text` — fixture is
      `@RestController\n@RequestMapping("/users")\npublic class
      UserController { ... }`. Assert `Symbol::source_text` starts
      with `@RestController` (not with `public class`). Test fails.
- [ ] **5.g:** Adjust the span computation: walk siblings backward
      from the class node and absorb preceding `marker_annotation` /
      `annotation` nodes (and modifiers) into the span. Tests pass.

### Task 6: Wire `.java` into `extract_for_path`

- [ ] **6.r:** `extract_for_path_routes_java_to_java_module` test
      in `lib.rs::tests`. Asserts a `.java` file produces non-empty
      symbols. Test fails (extension not yet dispatched).
- [ ] **6.g:** Add `pub mod java;` and the `.java` arm to the
      dispatch match. Test passes.

### Task 7: Add tree-sitter-kotlin workspace dep

- [ ] **7.r:** Same shape as Task 1. If the kotlin grammar fails to
      build against `tree-sitter = 0.22`, STOP, file the blocker
      explicitly in the report, and skip Tasks 8–11. v0.4 ships Java
      only.

### Task 8: Kotlin extractor — class + sealed + data + object

- [ ] **8.r:** `extracts_data_class`, `extracts_sealed_class_kt`,
      `extracts_object_as_class`, `extracts_companion_object_as_class`.
      Stub returns empty. Tests fail.
- [ ] **8.g:** Implement using kotlin grammar queries.
      All variants map to `SymbolKind::Class`. Tests pass.

### Task 9: Kotlin extractor — top-level fn vs member fn

- [ ] **9.r:** `extracts_top_level_function_as_function_kind` and
      `extracts_member_function_as_method_kind`. Tests fail.
- [ ] **9.g:** Implement: top-level `function_declaration` →
      `SymbolKind::Function`; nested inside class/interface/object →
      `SymbolKind::Method`. Tests pass.

### Task 10: Kotlin extractor — annotations preserved in source_text

- [ ] **10.r:** `preserves_annotations_in_source_text_kt` — fixture
      is `@Component\n@Singleton\nclass FooService { ... }`. Assert
      source_text begins with `@Component`. Test fails.
- [ ] **10.g:** Same span-extension logic as Java (Task 5), adapted
      for Kotlin's `annotation` AST node names. Tests pass.

### Task 11: Wire `.kt` and `.kts` into `extract_for_path`

- [ ] **11.r:** `extract_for_path_routes_kt_and_kts_to_kotlin_module`.
      Test fails.
- [ ] **11.g:** Add `pub mod kotlin;` and the `.kt` / `.kts` arms
      to the dispatch match. Test passes.

### Task 12: Cross-cutting — chunker round-trip on Java + Kotlin

- [ ] **12.r:** `chunker_merges_small_java_methods_up_to_500_tokens`
      and `chunker_emits_kotlin_data_classes_as_chunks`. Use the
      existing `chunk_symbols` plus extractor output. Tests fail
      (unless the chunker happens to "just work" — likely it does).
- [ ] **12.g:** No code change expected — the chunker is
      language-agnostic. If the test passes immediately, fold red
      and green into a single regression-test commit (precedent set
      by Track C deviation 1).

### Task 13: Spring-flavored integration fixture

- [ ] **13.r:** New file `crates/ohara-parse/src/tests/spring_fixture.rs`
      (or inline test in `lib.rs`). Build a small in-memory Spring-
      flavored Java source string (`@RestController` class with
      `@GetMapping` methods). Run `extract_for_path`. Assert each
      annotation appears verbatim in the relevant symbol's
      `source_text`. Test fails until Tasks 5/10 lands cleanly.
- [ ] **13.g:** Likely passes without further code if Tasks 5/10 are
      done correctly. Fold to single commit if so.

### Task 14: Final pass

- [ ] `cargo fmt --all && cargo clippy --workspace --all-targets --
      -D warnings && cargo test --workspace`. All clean. Update
      README to add Java + Kotlin to the supported-languages list (a
      one-line addition; v0.3 README doesn't currently enumerate, so
      add a small "Languages: Rust, Python, Java, Kotlin" line in the
      relevant section).

## 4. Done when

- All 14 tasks complete; final pass green.
- `cargo test --workspace` includes ≥ 14 new Java/Kotlin unit tests +
  the dispatch routing test + the cross-cutting chunker test.
- A real `.java` file (e.g. checked-in fixture or a manual smoke
  test) produces correct symbol output.
- README mentions Java + Kotlin.

## 5. Risk / fallback

- **Kotlin grammar dead-end.** If no actively-maintained
  `tree-sitter-kotlin` crate compiles against `tree-sitter = 0.22`,
  ship Java only (Tasks 1–6, 12–14 with Kotlin tests skipped). v0.4
  becomes a Java-support release; Kotlin moves to v0.4.1 or v0.5
  alongside whatever grammar work is needed.
- **Annotation span computation surprises.** Tree-sitter's modifier
  handling may differ between Java and Kotlin. If absorbing preceding
  annotations into the span is awkward in one language, document the
  deviation in the per-language module and adjust the spec rather
  than reverting the design.

## Files touched (consolidated)

- `Cargo.toml`
- `crates/ohara-parse/Cargo.toml`
- `crates/ohara-parse/src/lib.rs`
- `crates/ohara-parse/src/java.rs` (new)
- `crates/ohara-parse/src/kotlin.rs` (new)
- `README.md` (one-line update)
