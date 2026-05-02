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

## Chunking

All four languages share the v0.3 AST sibling-merge chunker
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

## Future languages

Tracked on the [Roadmap](../roadmap.md). New languages are mostly a
matter of adding a tree-sitter grammar, a node-kind → symbol-kind
mapping, and a fixture file to the per-language unit tests in
`crates/ohara-parse/`. Annotations / decorators should follow the
"keep them in `source_text`" rule so the retrieval-side query story
stays consistent.
