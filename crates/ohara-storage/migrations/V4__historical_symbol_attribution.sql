-- Plan 11 (v0.7): historical per-hunk symbol attribution + a normalized
-- semantic_text representation that the embedder + a new BM25 lane see
-- in place of the raw diff.
--
-- The raw diff stays in `hunk.diff_text` (display / provenance); the
-- new `hunk.semantic_text` is what the embedder + `fts_hunk_semantic`
-- see (search-time only).
--
-- `hunk_symbol` resolves the attribution-confidence question that
-- file-level joins can't: which symbol(s) inside the file did this
-- hunk actually touch? Three confidences are recorded:
--   exact_span    — a changed line intersects a parsed symbol's span
--   hunk_header   — git's hunk header named an enclosing function/class
--   file_fallback — kept for forward-compatibility; v0.7 indexer never writes it

ALTER TABLE hunk ADD COLUMN semantic_text TEXT NOT NULL DEFAULT '';

CREATE TABLE hunk_symbol (
    hunk_id INTEGER NOT NULL REFERENCES hunk(id) ON DELETE CASCADE,
    symbol_kind TEXT NOT NULL,
    symbol_name TEXT NOT NULL,
    qualified_name TEXT,
    attribution_kind TEXT NOT NULL,
    PRIMARY KEY (hunk_id, symbol_kind, symbol_name)
);
CREATE INDEX idx_hunk_symbol_name ON hunk_symbol (symbol_name);

CREATE VIRTUAL TABLE fts_hunk_semantic USING fts5(hunk_id UNINDEXED, content);

-- Backfill: every existing hunk gets semantic_text = diff_text so the
-- new FTS lane returns *something* immediately after migration. The
-- indexer overwrites this with a real semantic-text build on the next
-- pass that touches the hunk. No hunk_symbol rows are backfilled —
-- file-level / HEAD-symbol fallback continues to work via
-- bm25_hunks_by_symbol_name until a fresh index pass populates the
-- new table.
UPDATE hunk SET semantic_text = diff_text WHERE semantic_text = '';
INSERT INTO fts_hunk_semantic (hunk_id, content)
  SELECT id, semantic_text FROM hunk;
