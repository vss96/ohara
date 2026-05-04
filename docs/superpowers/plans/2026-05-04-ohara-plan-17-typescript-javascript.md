# TypeScript + JavaScript language support implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add tree-sitter-based symbol extraction for TypeScript (`.ts`, `.tsx`) and JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`) so `find_pattern`'s symbol-name BM25 lane fires on TS/JS code, matching the existing depth of Rust/Python/Java/Kotlin support.

**Architecture:** Two new modules under `crates/ohara-parse/src/languages/` — `javascript.rs` and `typescript.rs` — each exposing `pub fn extract(path, source, blob_sha) -> Result<Vec<Symbol>>` that mirrors the shape of `python.rs`. Tree-sitter queries live in `crates/ohara-parse/queries/javascript.scm` and `typescript.scm`. The extraction loop, dedup-by-span, and `Symbol` shape are identical to the existing language modules; only the grammar and node-name mappings change. TSX uses `tree_sitter_typescript::language_tsx()` (different grammar from `language_typescript()`), so `.tsx` routes through a thin variant of the TS extractor that swaps the grammar handle.

**Tech Stack:** Rust 2021, `tree-sitter` 0.22, `tree-sitter-typescript` 0.21+, `tree-sitter-javascript` 0.21+, existing `Symbol` / `SymbolKind` types in `ohara-core::types`.

**Out of scope:** C# (separate plan-18 — different grammar family, more edge cases). Decorators (TS legacy + stage-3) as separate symbol kinds. Ambient declarations (`declare module`, `.d.ts` typings) — these will be parsed but produce few useful symbols; defer.

---

## File structure

| File | Status | Responsibility |
|---|---|---|
| `Cargo.toml` (workspace) | Modify | Add `tree-sitter-typescript` and `tree-sitter-javascript` to `[workspace.dependencies]`. |
| `crates/ohara-parse/Cargo.toml` | Modify | Reference the two new deps via `dep.workspace = true`. |
| `crates/ohara-parse/src/languages/mod.rs` | Modify | `pub mod javascript;` and `pub mod typescript;` |
| `crates/ohara-parse/src/languages/javascript.rs` | Create | `extract` for `.js` / `.jsx` / `.mjs` / `.cjs`. |
| `crates/ohara-parse/src/languages/typescript.rs` | Create | `extract` for `.ts` and `.tsx` (two grammar handles). |
| `crates/ohara-parse/queries/javascript.scm` | Create | Tree-sitter query: function/class/method/arrow-const patterns. |
| `crates/ohara-parse/queries/typescript.scm` | Create | TS query: JS patterns + interface / type alias / enum. |
| `crates/ohara-parse/src/lib.rs` | Modify | New match arms in `extract_atomic_symbols`; new entries in `parser_versions()`. |
| `docs-book/src/architecture/languages.md` | Modify | List TS/JS in the supported-languages section. |
| `README.md` | Modify | Update language mention if any (it currently lists 4). |

`CHUNKER_VERSION` does **not** change — adding languages does not change AST sibling-merge semantics.

---

## Phase 1 — Workspace deps

### Task 1.1: Add tree-sitter grammar deps to workspace

**Files:**
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Add the two grammars under `[workspace.dependencies]`.**

Around line 41 (next to `tree-sitter-rust`, etc.), add:

```toml
tree-sitter-javascript = "0.21"
tree-sitter-typescript = "0.21"
```

- [ ] **Step 2: Verify the workspace resolves.**

Run: `cargo metadata --format-version 1 --offline >/dev/null 2>&1 || cargo metadata --format-version 1 >/dev/null`
Expected: exits 0. (`cargo metadata` is enough — we don't need a full build yet.)

- [ ] **Step 3: Commit.**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build(parse): add tree-sitter-javascript + tree-sitter-typescript to workspace"
```

### Task 1.2: Wire grammars into `ohara-parse` and create empty modules

**Files:**
- Modify: `crates/ohara-parse/Cargo.toml`
- Modify: `crates/ohara-parse/src/languages/mod.rs`
- Create: `crates/ohara-parse/src/languages/javascript.rs`
- Create: `crates/ohara-parse/src/languages/typescript.rs`
- Create: `crates/ohara-parse/queries/javascript.scm`
- Create: `crates/ohara-parse/queries/typescript.scm`

- [ ] **Step 1: Add deps in the crate's Cargo.toml.**

Under `[dependencies]`, alongside the other tree-sitter grammars:

```toml
tree-sitter-javascript = { workspace = true }
tree-sitter-typescript = { workspace = true }
```

- [ ] **Step 2: Re-export the modules.**

Replace `crates/ohara-parse/src/languages/mod.rs` with:

```rust
pub mod java;
pub mod javascript;
pub mod kotlin;
pub mod python;
pub mod rust;
pub mod typescript;
```

(Alphabetical, matching the existing convention in the file.)

- [ ] **Step 3: Create skeleton extractor for JavaScript.**

Write `crates/ohara-parse/src/languages/javascript.rs`:

```rust
use anyhow::Result;
use ohara_core::types::Symbol;

const QUERY_SRC: &str = include_str!("../../queries/javascript.scm");

pub fn extract(_file_path: &str, _source: &str, _blob_sha: &str) -> Result<Vec<Symbol>> {
    // Implemented in Phase 2.
    let _ = QUERY_SRC;
    Ok(vec![])
}
```

- [ ] **Step 4: Create skeleton extractor for TypeScript.**

Write `crates/ohara-parse/src/languages/typescript.rs`:

```rust
use anyhow::Result;
use ohara_core::types::Symbol;

const QUERY_SRC: &str = include_str!("../../queries/typescript.scm");

/// Discriminator for the two grammar handles inside `tree-sitter-typescript`:
/// `language_typescript()` parses `.ts`; `language_tsx()` parses `.tsx`.
#[derive(Debug, Clone, Copy)]
pub enum TsFlavor {
    Ts,
    Tsx,
}

pub fn extract(
    _file_path: &str,
    _source: &str,
    _blob_sha: &str,
    _flavor: TsFlavor,
) -> Result<Vec<Symbol>> {
    // Implemented in Phase 3.
    let _ = QUERY_SRC;
    Ok(vec![])
}
```

- [ ] **Step 5: Create empty query files.**

Create both as empty files for now (just `touch`):

```bash
touch crates/ohara-parse/queries/javascript.scm crates/ohara-parse/queries/typescript.scm
```

- [ ] **Step 6: Build the crate to verify scaffolding compiles.**

Run: `cargo build -p ohara-parse`
Expected: clean build, no warnings. (Empty `.scm` files are fine — `include_str!` reads them but the constants are unused inside skeletons via `let _ = ...`.)

- [ ] **Step 7: Commit.**

```bash
git add crates/ohara-parse/Cargo.toml \
        crates/ohara-parse/src/languages/mod.rs \
        crates/ohara-parse/src/languages/javascript.rs \
        crates/ohara-parse/src/languages/typescript.rs \
        crates/ohara-parse/queries/javascript.scm \
        crates/ohara-parse/queries/typescript.scm
git commit -m "build(parse): scaffold typescript + javascript modules"
```

---

## Phase 2 — JavaScript extractor (TDD)

Reference implementation: `crates/ohara-parse/src/languages/python.rs:1-138`. The shape — single tree-sitter query, single match loop with separate per-kind name+range trackers, dedup by `(span_start, span_end)` preferring more-specific kinds — is the canonical pattern. Mirror it exactly.

### Task 2.1: Failing test — top-level function declarations

**Files:**
- Modify: `crates/ohara-parse/src/languages/javascript.rs`

- [ ] **Step 1: Add a failing test.**

Append to `javascript.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_top_level_function_declarations() {
        let src = "function alpha() { return 1; }\nfunction beta(x) { return x; }\n";
        let syms = extract("a.js", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha missing: {names:?}");
        assert!(names.contains(&"beta"), "beta missing: {names:?}");
        for s in &syms {
            assert_eq!(s.language, "javascript");
            assert_eq!(s.file_path, "a.js");
            assert_eq!(s.blob_sha, "deadbeef");
        }
    }
}
```

- [ ] **Step 2: Run the test and confirm it fails.**

Run: `cargo test -p ohara-parse --lib languages::javascript::tests::extracts_top_level_function_declarations -- --nocapture`
Expected: FAIL — `assert!(names.contains(&"alpha"))` panics because the skeleton returns `Ok(vec![])`.

### Task 2.2: Implement function-declaration extraction

**Files:**
- Modify: `crates/ohara-parse/queries/javascript.scm`
- Modify: `crates/ohara-parse/src/languages/javascript.rs`

- [ ] **Step 1: Write the JavaScript query.**

Replace `crates/ohara-parse/queries/javascript.scm` with:

```scheme
(function_declaration name: (identifier) @func_name) @def_function
```

(Tree-sitter-javascript node names: `function_declaration` for `function foo() {}`. Verify with `tree-sitter parse a.js` if needed — the node name has been stable since 0.20.)

- [ ] **Step 2: Implement `extract` against the query.**

Replace the body of `extract` in `javascript.rs` with this (mirrors `python.rs:8-99` but stripped to the single capture pair):

```rust
pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    use anyhow::Context;
    use ohara_core::types::SymbolKind;
    use std::collections::HashMap;
    use tree_sitter::{Parser, Query, QueryCursor};

    let mut parser = Parser::new();
    let language = tree_sitter_javascript::language();
    parser
        .set_language(&language)
        .context("set javascript language")?;
    let tree = parser.parse(source, None).context("parse javascript")?;
    let query = Query::new(&language, QUERY_SRC).context("javascript query")?;
    let mut cursor = QueryCursor::new();

    let mut out: Vec<Symbol> = Vec::new();

    for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
        let mut func_name: Option<String> = None;
        let mut func_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "func_name" => {
                    func_name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "def_function" => {
                    func_range = Some((n.start_byte(), n.end_byte()));
                }
                _ => {}
            }
        }

        if let (Some(name), Some((s, e))) = (func_name, func_range) {
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "javascript".to_string(),
                kind: SymbolKind::Function,
                name,
                qualified_name: None,
                sibling_names: Vec::new(),
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: source[s..e].to_string(),
            });
        }
    }

    let _: HashMap<(u32, u32), Symbol> = HashMap::new(); // dedup added in 2.4
    Ok(out)
}
```

Update the imports at the top of the file to drop the now-unused stub imports and add only what `extract` needs:

```rust
use anyhow::Result;
use ohara_core::types::Symbol;
```

- [ ] **Step 3: Run the test and confirm it passes.**

Run: `cargo test -p ohara-parse --lib languages::javascript::tests::extracts_top_level_function_declarations`
Expected: PASS.

- [ ] **Step 4: Commit.**

```bash
git add crates/ohara-parse/queries/javascript.scm crates/ohara-parse/src/languages/javascript.rs
git commit -m "feat(parse): javascript function_declaration extraction"
```

### Task 2.3: Failing test — class declarations + methods

**Files:**
- Modify: `crates/ohara-parse/src/languages/javascript.rs`

- [ ] **Step 1: Add a failing test.**

Inside the `tests` module:

```rust
    #[test]
    fn extracts_class_and_method_declarations() {
        let src = "class Foo {\n  bar() { return 1; }\n  baz(x) { return x; }\n}\n";
        let syms = extract("a.js", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "Foo class missing: {names:?}");
        assert!(names.contains(&"bar"), "bar method missing: {names:?}");
        assert!(names.contains(&"baz"), "baz method missing: {names:?}");
        let foo = syms.iter().find(|s| s.name == "Foo").unwrap();
        assert!(matches!(foo.kind, ohara_core::types::SymbolKind::Class));
        let bar = syms.iter().find(|s| s.name == "bar").unwrap();
        assert!(matches!(bar.kind, ohara_core::types::SymbolKind::Method));
    }
```

- [ ] **Step 2: Run and confirm it fails.**

Run: `cargo test -p ohara-parse --lib languages::javascript::tests::extracts_class_and_method_declarations`
Expected: FAIL — current query only matches `function_declaration`.

### Task 2.4: Add class + method patterns and dedup

**Files:**
- Modify: `crates/ohara-parse/queries/javascript.scm`
- Modify: `crates/ohara-parse/src/languages/javascript.rs`

- [ ] **Step 1: Extend the query.**

Replace `javascript.scm` with:

```scheme
(function_declaration name: (identifier) @func_name) @def_function

(class_declaration
  name: (identifier) @class_name
  body: (class_body
    (method_definition name: (property_identifier) @method_name) @def_method)) @def_class
```

- [ ] **Step 2: Extend `extract` to handle the new captures + dedup.**

Replace the match-loop body in `javascript.rs` with this (mirrors `python.rs:31-122` exactly, swapping `python` → `javascript`):

```rust
    for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
        let mut class_name: Option<String> = None;
        let mut class_range: Option<(usize, usize)> = None;
        let mut method_name: Option<String> = None;
        let mut method_range: Option<(usize, usize)> = None;
        let mut func_name: Option<String> = None;
        let mut func_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "func_name" => func_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "method_name" => method_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "class_name" => class_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "def_function" => func_range = Some((n.start_byte(), n.end_byte())),
                "def_method" => method_range = Some((n.start_byte(), n.end_byte())),
                "def_class" => class_range = Some((n.start_byte(), n.end_byte())),
                _ => {}
            }
        }

        if let (Some(name), Some((s, e))) = (class_name, class_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Class, name, s, e, source));
        }
        if let (Some(name), Some((s, e))) = (method_name, method_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Method, name, s, e, source));
        }
        if let (Some(name), Some((s, e))) = (func_name, func_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Function, name, s, e, source));
        }
    }

    // Dedupe by (span_start, span_end). When the same span is captured by
    // multiple patterns, prefer Method/Class over Function.
    let mut by_span: HashMap<(u32, u32), Symbol> = HashMap::new();
    for sym in out {
        let key = (sym.span_start, sym.span_end);
        match by_span.get(&key) {
            None => {
                by_span.insert(key, sym);
            }
            Some(existing) => {
                if existing.kind == SymbolKind::Function
                    && (sym.kind == SymbolKind::Method || sym.kind == SymbolKind::Class)
                {
                    by_span.insert(key, sym);
                }
            }
        }
    }
    Ok(by_span.into_values().collect())
}

fn make_symbol(
    file_path: &str,
    blob_sha: &str,
    kind: SymbolKind,
    name: String,
    s: usize,
    e: usize,
    source: &str,
) -> Symbol {
    Symbol {
        file_path: file_path.to_string(),
        language: "javascript".to_string(),
        kind,
        name,
        qualified_name: None,
        sibling_names: Vec::new(),
        span_start: s as u32,
        span_end: e as u32,
        blob_sha: blob_sha.to_string(),
        source_text: source[s..e].to_string(),
    }
}
```

(Hoisting the `Symbol` builder into `make_symbol` keeps each insertion site to one line and matches a refactor that the JS path will need anyway in Task 2.6 below.)

- [ ] **Step 3: Run both tests and confirm they pass.**

Run: `cargo test -p ohara-parse --lib languages::javascript::tests`
Expected: 2 passed.

- [ ] **Step 4: Commit.**

```bash
git add crates/ohara-parse/queries/javascript.scm crates/ohara-parse/src/languages/javascript.rs
git commit -m "feat(parse): javascript class + method extraction"
```

### Task 2.5: Failing test — arrow function const

**Files:**
- Modify: `crates/ohara-parse/src/languages/javascript.rs`

- [ ] **Step 1: Add a failing test.**

Inside the `tests` module:

```rust
    #[test]
    fn extracts_arrow_function_const() {
        let src = "const handle = (req, res) => { return res.json({}); };\n\
                   export const greet = name => `hi ${name}`;\n";
        let syms = extract("a.js", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"handle"), "handle missing: {names:?}");
        assert!(names.contains(&"greet"), "greet missing: {names:?}");
        let handle = syms.iter().find(|s| s.name == "handle").unwrap();
        assert!(matches!(handle.kind, ohara_core::types::SymbolKind::Function));
    }
```

- [ ] **Step 2: Run and confirm it fails.**

Run: `cargo test -p ohara-parse --lib languages::javascript::tests::extracts_arrow_function_const`
Expected: FAIL — query has no pattern for `lexical_declaration` + `variable_declarator` + `arrow_function`.

### Task 2.6: Add arrow-function-const pattern

**Files:**
- Modify: `crates/ohara-parse/queries/javascript.scm`
- Modify: `crates/ohara-parse/src/languages/javascript.rs`

- [ ] **Step 1: Extend the query.**

Append to `javascript.scm`:

```scheme
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (arrow_function))) @def_arrow

(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (function_expression))) @def_arrow
```

(`function_expression` covers `const f = function(...){}`. `arrow_function` covers the `=>` form. The export-prefixed form is wrapped in an `export_statement` that contains the `lexical_declaration` — tree-sitter's matcher descends into it automatically, so no separate pattern is needed.)

- [ ] **Step 2: Add the new captures to the match loop.**

Inside the `for m in cursor.matches(...)` loop, add fields for arrow tracking and capture handling. Replace the local-variable block + `for cap in m.captures` arm to include arrows:

```rust
        let mut arrow_name: Option<String> = None;
        let mut arrow_range: Option<(usize, usize)> = None;
```

(Add at the top of the loop alongside `class_name`, etc.)

```rust
                "arrow_name" => arrow_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "def_arrow" => arrow_range = Some((n.start_byte(), n.end_byte())),
```

(Add inside the inner `match cap_name`.)

After the existing `if let (Some(name), Some((s, e))) = (func_name, func_range)` block, add:

```rust
        if let (Some(name), Some((s, e))) = (arrow_name, arrow_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Function, name, s, e, source));
        }
```

- [ ] **Step 3: Run all JS tests.**

Run: `cargo test -p ohara-parse --lib languages::javascript::tests`
Expected: 3 passed.

- [ ] **Step 4: Commit.**

```bash
git add crates/ohara-parse/queries/javascript.scm crates/ohara-parse/src/languages/javascript.rs
git commit -m "feat(parse): javascript arrow-function const extraction"
```

### Task 2.7: Wire JavaScript file extensions into `extract_atomic_symbols`

**Files:**
- Modify: `crates/ohara-parse/src/lib.rs`

- [ ] **Step 1: Failing test for the dispatch.**

Append to the existing `tests` module in `crates/ohara-parse/src/lib.rs` (find it near the `extract_atomic_symbols` definition; add a sibling test):

```rust
    #[test]
    fn extract_atomic_symbols_dispatches_javascript_extensions() {
        let src = "function alpha() {}\n";
        for ext in ["js", "jsx", "mjs", "cjs"] {
            let path = format!("a.{ext}");
            let syms = extract_atomic_symbols(&path, src, "deadbeef").expect("dispatch");
            assert!(
                syms.iter().any(|s| s.name == "alpha"),
                "{ext} did not dispatch to javascript extractor: {syms:?}"
            );
        }
    }
```

- [ ] **Step 2: Run and confirm fail.**

Run: `cargo test -p ohara-parse --lib extract_atomic_symbols_dispatches_javascript_extensions`
Expected: FAIL — `_ => return Ok(vec![])` arm catches `.js`.

- [ ] **Step 3: Add match arms.**

In `extract_atomic_symbols` (line ~89), insert above the `_ => return Ok(vec![])` arm:

```rust
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => {
            languages::javascript::extract(path, source, blob_sha)?
        }
```

- [ ] **Step 4: Run and confirm pass.**

Run: `cargo test -p ohara-parse --lib extract_atomic_symbols_dispatches_javascript_extensions`
Expected: PASS.

- [ ] **Step 5: Add `javascript` to `parser_versions()`.**

In `lib.rs` line ~53:

```rust
pub fn parser_versions() -> BTreeMap<String, String> {
    [
        ("rust", "1"),
        ("python", "1"),
        ("java", "1"),
        ("kotlin", "1"),
        ("javascript", "1"),
    ]
    .into_iter()
    .map(|(lang, ver)| (lang.to_string(), ver.to_string()))
    .collect()
}
```

- [ ] **Step 6: Run the full crate test suite.**

Run: `cargo test -p ohara-parse`
Expected: all green.

- [ ] **Step 7: Commit.**

```bash
git add crates/ohara-parse/src/lib.rs
git commit -m "feat(parse): dispatch .js/.jsx/.mjs/.cjs to javascript extractor"
```

---

## Phase 3 — TypeScript extractor (TDD)

The TS extractor reuses the JS pattern set (TS is a JS superset) and adds `interface_declaration`, `type_alias_declaration`, and `enum_declaration`. The grammar handle differs by file extension: `.ts` uses `tree_sitter_typescript::language_typescript()`, `.tsx` uses `tree_sitter_typescript::language_tsx()`. Both grammars accept the same query source for the symbols we care about (the JSX node types added in TSX don't affect symbol extraction — JSX components are still `function_declaration` / `lexical_declaration` nodes).

### Task 3.1: Failing test — TS function + class

**Files:**
- Modify: `crates/ohara-parse/src/languages/typescript.rs`

- [ ] **Step 1: Add a failing test.**

Append to `typescript.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_function_and_class_from_ts() {
        let src = "function alpha(): number { return 1; }\n\
                   class Foo {\n  bar(x: number): number { return x; }\n}\n";
        let syms = extract("a.ts", src, "deadbeef", TsFlavor::Ts).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha missing: {names:?}");
        assert!(names.contains(&"Foo"), "Foo missing: {names:?}");
        assert!(names.contains(&"bar"), "bar missing: {names:?}");
        for s in &syms {
            assert_eq!(s.language, "typescript");
        }
    }
}
```

- [ ] **Step 2: Run and confirm it fails.**

Run: `cargo test -p ohara-parse --lib languages::typescript::tests::extracts_function_and_class_from_ts`
Expected: FAIL — skeleton returns empty.

### Task 3.2: Implement TS base extractor + share helper with JS

**Files:**
- Modify: `crates/ohara-parse/queries/typescript.scm`
- Modify: `crates/ohara-parse/src/languages/typescript.rs`

- [ ] **Step 1: Seed the TypeScript query.**

Write `crates/ohara-parse/queries/typescript.scm` with the same patterns as `javascript.scm`:

```scheme
(function_declaration name: (identifier) @func_name) @def_function

(class_declaration
  name: (type_identifier) @class_name
  body: (class_body
    (method_definition name: (property_identifier) @method_name) @def_method)) @def_class

(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (arrow_function))) @def_arrow

(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (function_expression))) @def_arrow
```

(Note: TS uses `type_identifier` for class names, not `identifier`. The other node names match JS.)

- [ ] **Step 2: Implement `extract` for TS.**

Replace `typescript.rs`'s `extract` body:

```rust
use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use std::collections::HashMap;
use tree_sitter::{Language, Parser, Query, QueryCursor};

const QUERY_SRC: &str = include_str!("../../queries/typescript.scm");

#[derive(Debug, Clone, Copy)]
pub enum TsFlavor {
    Ts,
    Tsx,
}

fn language_for(flavor: TsFlavor) -> Language {
    match flavor {
        TsFlavor::Ts => tree_sitter_typescript::language_typescript(),
        TsFlavor::Tsx => tree_sitter_typescript::language_tsx(),
    }
}

pub fn extract(
    file_path: &str,
    source: &str,
    blob_sha: &str,
    flavor: TsFlavor,
) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    let language = language_for(flavor);
    parser
        .set_language(&language)
        .context("set typescript language")?;
    let tree = parser.parse(source, None).context("parse typescript")?;
    let query = Query::new(&language, QUERY_SRC).context("typescript query")?;
    let mut cursor = QueryCursor::new();

    let mut out: Vec<Symbol> = Vec::new();

    for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
        let mut class_name: Option<String> = None;
        let mut class_range: Option<(usize, usize)> = None;
        let mut method_name: Option<String> = None;
        let mut method_range: Option<(usize, usize)> = None;
        let mut func_name: Option<String> = None;
        let mut func_range: Option<(usize, usize)> = None;
        let mut arrow_name: Option<String> = None;
        let mut arrow_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "func_name" => func_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "method_name" => method_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "class_name" => class_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "arrow_name" => arrow_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "def_function" => func_range = Some((n.start_byte(), n.end_byte())),
                "def_method" => method_range = Some((n.start_byte(), n.end_byte())),
                "def_class" => class_range = Some((n.start_byte(), n.end_byte())),
                "def_arrow" => arrow_range = Some((n.start_byte(), n.end_byte())),
                _ => {}
            }
        }

        if let (Some(name), Some((s, e))) = (class_name, class_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Class, name, s, e, source));
        }
        if let (Some(name), Some((s, e))) = (method_name, method_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Method, name, s, e, source));
        }
        if let (Some(name), Some((s, e))) = (func_name, func_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Function, name, s, e, source));
        }
        if let (Some(name), Some((s, e))) = (arrow_name, arrow_range) {
            out.push(make_symbol(file_path, blob_sha, SymbolKind::Function, name, s, e, source));
        }
    }

    let mut by_span: HashMap<(u32, u32), Symbol> = HashMap::new();
    for sym in out {
        let key = (sym.span_start, sym.span_end);
        match by_span.get(&key) {
            None => {
                by_span.insert(key, sym);
            }
            Some(existing) => {
                if existing.kind == SymbolKind::Function
                    && (sym.kind == SymbolKind::Method || sym.kind == SymbolKind::Class)
                {
                    by_span.insert(key, sym);
                }
            }
        }
    }
    Ok(by_span.into_values().collect())
}

fn make_symbol(
    file_path: &str,
    blob_sha: &str,
    kind: SymbolKind,
    name: String,
    s: usize,
    e: usize,
    source: &str,
) -> Symbol {
    Symbol {
        file_path: file_path.to_string(),
        language: "typescript".to_string(),
        kind,
        name,
        qualified_name: None,
        sibling_names: Vec::new(),
        span_start: s as u32,
        span_end: e as u32,
        blob_sha: blob_sha.to_string(),
        source_text: source[s..e].to_string(),
    }
}
```

- [ ] **Step 3: Run the test.**

Run: `cargo test -p ohara-parse --lib languages::typescript::tests::extracts_function_and_class_from_ts`
Expected: PASS.

- [ ] **Step 4: Commit.**

```bash
git add crates/ohara-parse/queries/typescript.scm crates/ohara-parse/src/languages/typescript.rs
git commit -m "feat(parse): typescript function/class/method/arrow extraction"
```

### Task 3.3: Failing test — interfaces, type aliases, enums

**Files:**
- Modify: `crates/ohara-parse/src/languages/typescript.rs`

- [ ] **Step 1: Add a failing test.**

```rust
    #[test]
    fn extracts_interface_type_alias_and_enum() {
        let src = "interface Greeter { hello(): string; }\n\
                   type UserId = number;\n\
                   enum Status { Active, Inactive }\n";
        let syms = extract("a.ts", src, "deadbeef", TsFlavor::Ts).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Greeter"), "interface missing: {names:?}");
        assert!(names.contains(&"UserId"), "type alias missing: {names:?}");
        assert!(names.contains(&"Status"), "enum missing: {names:?}");
        for n in ["Greeter", "UserId", "Status"] {
            let s = syms.iter().find(|s| s.name == n).unwrap();
            assert!(matches!(s.kind, ohara_core::types::SymbolKind::Class),
                "expected Class kind for {n}, got {:?}", s.kind);
        }
    }
```

- [ ] **Step 2: Run and confirm fail.**

Run: `cargo test -p ohara-parse --lib languages::typescript::tests::extracts_interface_type_alias_and_enum`
Expected: FAIL — query lacks the three patterns.

### Task 3.4: Add interface / type-alias / enum patterns

**Files:**
- Modify: `crates/ohara-parse/queries/typescript.scm`
- Modify: `crates/ohara-parse/src/languages/typescript.rs`

- [ ] **Step 1: Append to the TypeScript query.**

Append to `typescript.scm`:

```scheme
(interface_declaration name: (type_identifier) @class_name) @def_class
(type_alias_declaration name: (type_identifier) @class_name) @def_class
(enum_declaration name: (identifier) @class_name) @def_class
```

(Re-using `class_name` / `def_class` capture names means **no Rust changes** are needed — these all flow through the existing `SymbolKind::Class` insertion arm. The "interface = class for symbol-search purposes" decision is documented in the plan header.)

- [ ] **Step 2: Run the test.**

Run: `cargo test -p ohara-parse --lib languages::typescript::tests::extracts_interface_type_alias_and_enum`
Expected: PASS.

- [ ] **Step 3: Commit.**

```bash
git add crates/ohara-parse/queries/typescript.scm
git commit -m "feat(parse): typescript interface/type-alias/enum extraction"
```

### Task 3.5: Failing test — TSX components

**Files:**
- Modify: `crates/ohara-parse/src/languages/typescript.rs`

- [ ] **Step 1: Add a failing test using `TsFlavor::Tsx`.**

```rust
    #[test]
    fn extracts_tsx_components() {
        let src = "function App(): JSX.Element { return <div />; }\n\
                   const Button = (props: { label: string }) => <button>{props.label}</button>;\n";
        let syms = extract("a.tsx", src, "deadbeef", TsFlavor::Tsx).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"App"), "App component missing: {names:?}");
        assert!(names.contains(&"Button"), "Button component missing: {names:?}");
    }
```

- [ ] **Step 2: Run and confirm result.**

Run: `cargo test -p ohara-parse --lib languages::typescript::tests::extracts_tsx_components`
Expected: PASS already (the TSX grammar accepts the same query, and the extractor picks up the function declaration + the arrow const). If it FAILs, the grammar's TSX node names diverge from the TS ones — investigate by parsing with `tree-sitter` CLI and add TSX-specific nodes only as needed. **Stop here** if it passes; do not fabricate a TSX-specific code path that isn't necessary.

- [ ] **Step 3: Commit if any code changed.**

```bash
git add crates/ohara-parse/src/languages/typescript.rs
git commit -m "test(parse): tsx component extraction (no code change)"
```

(If only the test was added and it passed without code changes, this commit is just the test.)

### Task 3.6: Wire TS file extensions into `extract_atomic_symbols`

**Files:**
- Modify: `crates/ohara-parse/src/lib.rs`

- [ ] **Step 1: Failing test for the dispatch.**

```rust
    #[test]
    fn extract_atomic_symbols_dispatches_typescript_extensions() {
        let src = "function alpha() {}\n";
        for ext in ["ts", "tsx"] {
            let path = format!("a.{ext}");
            let syms = extract_atomic_symbols(&path, src, "deadbeef").expect("dispatch");
            assert!(
                syms.iter().any(|s| s.name == "alpha"),
                "{ext} did not dispatch to typescript extractor: {syms:?}"
            );
        }
    }
```

- [ ] **Step 2: Run and confirm fail.**

Run: `cargo test -p ohara-parse --lib extract_atomic_symbols_dispatches_typescript_extensions`
Expected: FAIL.

- [ ] **Step 3: Add match arms.**

In `extract_atomic_symbols`, above the `_ => return Ok(vec![])` arm:

```rust
        Some("ts") => languages::typescript::extract(path, source, blob_sha, languages::typescript::TsFlavor::Ts)?,
        Some("tsx") => languages::typescript::extract(path, source, blob_sha, languages::typescript::TsFlavor::Tsx)?,
```

- [ ] **Step 4: Add `typescript` to `parser_versions()`.**

```rust
        ("typescript", "1"),
```

(Insert in the array — alphabetical or grouped, your call; existing order is not strictly alphabetical so match the surrounding style.)

- [ ] **Step 5: Run the full crate test suite.**

Run: `cargo test -p ohara-parse`
Expected: all green.

- [ ] **Step 6: Commit.**

```bash
git add crates/ohara-parse/src/lib.rs
git commit -m "feat(parse): dispatch .ts/.tsx to typescript extractor"
```

---

## Phase 4 — End-to-end + docs

### Task 4.1: Workspace lint + test + clippy

- [ ] **Step 1: Format.**

Run: `cargo fmt --all`
Expected: no diff (or minimal trivial diff). If diff, commit it as `style: cargo fmt`.

- [ ] **Step 2: Clippy.**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 3: Full test suite.**

Run: `cargo test --workspace`
Expected: all green.

### Task 4.2: Plan-13 compatibility verdict — verify a refresh prompt fires

The new entries in `parser_versions()` mean any pre-plan-17 index is now on a stale `parser_javascript` / `parser_typescript` row (or missing them entirely). After the next index pass runs against an old DB, `ohara status` should report `query_compatible_needs_refresh` until `--force` is run.

- [ ] **Step 1: Manual smoke test.**

Run:

```bash
fixtures/build_tiny.sh
cargo run -p ohara-cli -- index fixtures/tiny/repo
cargo run -p ohara-cli -- status fixtures/tiny/repo
```

Expected: status reports `compatible` (the index was just built with this binary). The verdict mechanic is exercised by the plan-13 unit tests; this is a sanity check that the new parser-version rows actually persist.

- [ ] **Step 2: Inspect.**

```bash
sqlite3 ~/.ohara/$(cargo run -p ohara-cli -- status fixtures/tiny/repo --json | jq -r .repo_id)/db.sqlite \
  "SELECT component, version FROM index_metadata ORDER BY component;"
```

Expected: rows include `parser_javascript = 1` and `parser_typescript = 1` alongside the existing `parser_rust`, `parser_python`, `parser_java`, `parser_kotlin`.

(If the `--json` flag on status doesn't exist in the current build, hard-code a known repo id or read it from the indexer's stdout. The test plan item is "verify the new rows land", not "automate the introspection".)

### Task 4.3: Update the languages doc page

**Files:**
- Modify: `docs-book/src/architecture/languages.md`

- [ ] **Step 1: Read the current doc.**

Run: `cat docs-book/src/architecture/languages.md`
Note the existing list shape (Rust / Python / Java / Kotlin) so the additions match its prose style.

- [ ] **Step 2: Add TS + JS sections.**

Append entries that follow the existing per-language pattern. Each section should cover: file extensions matched, tree-sitter grammar used, what symbols are extracted (functions / classes / methods / + TS-specific), and what's intentionally not extracted (decorators, namespaces, ambient `.d.ts` declarations).

- [ ] **Step 3: Build the docs.**

Run: `(cd docs-book && mdbook build)`
Expected: clean build, no broken links.

- [ ] **Step 4: Commit.**

```bash
git add docs-book/src/architecture/languages.md
git commit -m "docs(arch): document typescript + javascript symbol extraction"
```

### Task 4.4: Update the README's languages mention

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Find the "Languages indexed" mention.**

Run: `grep -n "Rust, Python\|Languages\|tree-sitter" README.md`

- [ ] **Step 2: Update to include TypeScript and JavaScript.**

If the README currently says e.g. "Rust, Python, Java, Kotlin", change to "Rust, TypeScript, JavaScript, Python, Java, Kotlin" (audience-priority order).

- [ ] **Step 3: Commit.**

```bash
git add README.md
git commit -m "docs(readme): list typescript + javascript among indexed languages"
```

### Task 4.5: Open the PR

- [ ] **Step 1: Push the branch.**

```bash
git push -u origin feat/ts-js-language-support
```

- [ ] **Step 2: Open PR.**

Use `gh pr create` with a body that includes:
- Summary — adds TS/JS extractors mirroring `python.rs` patterns.
- Out-of-scope note — C# is plan-18.
- Test plan — every test added in this plan, plus the manual `index_metadata` smoke from Task 4.2.

---

## Self-review

**1. Spec coverage.** The spec is "TypeScript + JavaScript first-class support, mirroring existing language depth." Tasks cover:
- Workspace dep wiring (1.1, 1.2)
- JS function / class / method / arrow-const extraction (2.1–2.6)
- JS file-extension dispatch + parser-version metadata (2.7)
- TS function / class / method / arrow extraction (3.1–3.2)
- TS interface / type-alias / enum (3.3–3.4)
- TSX components (3.5)
- TS file-extension dispatch + parser-version metadata (3.6)
- Plan-13 metadata smoke (4.2)
- Docs updates (4.3, 4.4)

C# is explicitly deferred to plan-18.

**2. Placeholder scan.** No "TBD"/"implement later"/"add error handling" patterns. Each task with a code change shows the code. Tree-sitter query strings may need tuning if the grammar version differs from what the plan was written against — that's iteration, not a placeholder.

**3. Type consistency.** `Symbol`, `SymbolKind::{Function, Method, Class}`, and `make_symbol` are used identically across both extractors. `TsFlavor::{Ts, Tsx}` is defined in 1.2 and used in 3.2 / 3.6. `parser_versions()` shape (`("name", "ver")`) matches the existing pattern.

---

## Follow-up: plan-18 (C#)

C# support should land as a separate plan because:
- Different grammar family (`tree-sitter-c-sharp`).
- Heavier symbol model: partial classes (multi-file definitions), auto-implemented properties with backing fields, generics with variance, primary constructors (C# 12), records.
- Decorators-equivalent (attributes) sit before declarations and aren't currently modeled.

The plan-18 structure should mirror this one: workspace dep → scaffold → TDD on canonical idioms → wire `.cs` extension → docs. Estimated 1.5–2× the work of TS+JS combined, mostly due to partial-class handling.
