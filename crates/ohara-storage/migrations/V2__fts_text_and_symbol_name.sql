-- v0.3 (Plan 3): add FTS5 BM25 lanes and a sibling_names column for AST-merged chunks.

ALTER TABLE symbol ADD COLUMN sibling_names TEXT NOT NULL DEFAULT '[]';

CREATE VIRTUAL TABLE fts_hunk_text USING fts5(hunk_id UNINDEXED, content);
CREATE VIRTUAL TABLE fts_symbol_name USING fts5(symbol_id UNINDEXED, kind, name, sibling_names);

-- Backfill from existing rows so v0.2-era indexes are searchable on first run.
INSERT INTO fts_hunk_text (hunk_id, content)
  SELECT id, diff_text FROM hunk;

INSERT INTO fts_symbol_name (symbol_id, kind, name, sibling_names)
  SELECT id, kind, name, sibling_names FROM symbol;
