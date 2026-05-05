# Language support

ohara extracts HEAD-snapshot symbols (classes, methods, functions,
records, sealed types, …) from source files using
[`tree-sitter`](https://tree-sitter.github.io/tree-sitter/). The
extracted symbols feed the `vec_symbol`, `fts_symbol`, and
`fts_symbol_name` tables and contribute one of the three retrieval
lanes in [`find_pattern`](./retrieval-pipeline.md).

Hunks (the diff bodies that drive the other two retrieval lanes) are
language-agnostic — they're extracted by `git2` regardless of the file
type, so even unsupported languages still appear in
`find_pattern` results via the BM25 hunk-text and vector lanes. Symbol
extraction is the language-specific piece.

## Supported languages

| Language | Since | Notes |
|----------|-------|-------|
| Rust | v0.1 | Functions, methods, structs, enums, traits, impls. |
| Python | v0.1 | Classes, methods, top-level functions. |
| Java 17+ | v0.4 | Classes (incl. **sealed**), interfaces, **records**, enums, methods. |
| Kotlin 1.9 / 2.0 | v0.4 | Classes (incl. **sealed**), **data classes**, **objects** + companion objects, interfaces, top-level + member functions. |
| TypeScript | v0.8 | `.ts`, `.tsx`. Functions, classes, methods, arrow-function `const`s, **interfaces**, **type aliases**, **enums**. |
| JavaScript | v0.8 | `.js`, `.jsx`, `.mjs`, `.cjs`. Functions, classes, methods, arrow-function `const`s. |

## Java + Kotlin specifics

The v0.4 release was specifically designed to make ohara useful on
Spring-flavored codebases, which means:

- **Sealed types and records** (Java 17+) and **data classes / objects /
  companion objects** (Kotlin) are first-class symbol kinds.
- **Annotations stay inside `source_text`.** Decorators like
  `@RestController`, `@Service`, `@Component`,
  `@SpringBootApplication`, and Kotlin's `@Composable` /
  `@Serializable` are preserved verbatim in the symbol's
  `source_text` field. That means embeddings and BM25 both pick up
  Spring-style markers without any new query syntax — a query like
  "REST controller for user signup" matches symbols annotated with
  `@RestController` directly.

## TypeScript + JavaScript specifics

The v0.8 release adds first-class symbol extraction for the JS/TS
ecosystem via the
[`tree-sitter-typescript`](https://github.com/tree-sitter/tree-sitter-typescript)
0.23 and
[`tree-sitter-javascript`](https://github.com/tree-sitter/tree-sitter-javascript)
0.23 grammars. Both grammars are loaded through the
[`tree-sitter-language`](https://crates.io/crates/tree-sitter-language)
ABI shim so they keep working as the host `tree-sitter` runtime
advances.

- **TypeScript** dispatches on `.ts` and `.tsx`; the latter is parsed
  with the TSX dialect of the grammar so JSX expressions inside `.tsx`
  components don't derail symbol extraction.
- **JavaScript** dispatches on `.js`, `.jsx`, `.mjs`, and `.cjs`. JSX
  inside `.jsx` parses cleanly via the same grammar.
- **Symbols extracted (both languages):** `function` declarations,
  `class` declarations, methods (including `constructor`, `get`/`set`,
  `static`, and `async`), and arrow-function `const`s
  (`export const Foo = (…) => {…}`) — the last one matters because the
  modern React/Next idiom expresses components and hooks as arrow-bound
  consts rather than `function` declarations.
- **Symbols extracted (TypeScript only):** `interface` declarations,
  `type` aliases, and `enum` declarations.
- **Intentionally not extracted yet:** decorators (preserved inside
  `source_text` like Java/Kotlin annotations, but not surfaced as
  separate symbols), ambient declarations in `.d.ts` files (the file
  extension dispatches normally but `declare` blocks aren't
  symbolized), and `namespace` / `module` declarations (TS-only legacy
  module syntax). These are tracked on the roadmap.

## Chunking

All six languages share the v0.3 AST sibling-merge chunker
(implemented in `ohara-parse`): depth-first traversal of the
tree-sitter parse tree, accumulate sibling nodes into the current
chunk while the running token total stays under a 500-token budget
(token ≈ char_count / 4 for chunking decisions). When a single node
exceeds the budget, emit it alone — preserves semantic locality at
the cost of larger chunks for occasional huge functions.

The chunker also records sibling-symbol names in a JSON-encoded
`sibling_names` column on each `symbol` row. That column feeds the
`fts_symbol_name` BM25 lane, so a query for "X" matches not just
symbols literally named X but symbols that share a parent with one.

As of v0.6 the per-language extractors live under
`crates/ohara-parse/src/languages/` (one module per language) — same
behavior, tighter directory tree.

## Grammar and parser versions

Plan-18 (v0.7) modernized the tree-sitter stack: the workspace tracks
`tree-sitter` 0.25, the rust/python/java grammars were bumped to their
current upstream releases (rust 0.21 → 0.24, python 0.21 → 0.25, java
0.21 → 0.23), and the kotlin grammar was swapped from the abandoned
`fwcd/tree-sitter-kotlin` to the more-recently-maintained
[`tree-sitter-grammars/tree-sitter-kotlin-ng`](https://github.com/tree-sitter-grammars/tree-sitter-kotlin-ng)
fork.

All four `parser_versions` bumped from `"1"` to `"2"` in lockstep, so
indexes built before v0.7 receive a `query_compatible_needs_refresh`
verdict per plan-13 (`docs/superpowers/plans/2026-05-02-ohara-plan-13-index-metadata-and-rebuild-safety.md`):
queries still work, but the symbol/hunk derived rows can be
out-of-date. Recovery is `ohara index --force`, which re-walks HEAD
symbols and rewrites derived rows without touching the embedding
vectors (model + dimension are unchanged).

## Future languages

Tracked on the [Roadmap](../roadmap.md). New languages are mostly a
matter of adding a tree-sitter grammar, a node-kind → symbol-kind
mapping, and a fixture file to the per-language unit tests in
`crates/ohara-parse/`. Annotations / decorators should follow the
"keep them in `source_text`" rule so the retrieval-side query story
stays consistent.
