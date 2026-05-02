-- Plan 13 (v0.7): record per-component metadata about how the index was
-- built so the runtime can detect when an old index is incompatible
-- with the current binary's embedder / chunker / parser / semantic-text
-- versions.
--
-- Keyed by (repo_id, component). `version` is the recorded version
-- string (free-form per component); `value_json` carries any extra
-- shape (e.g. embedding dimension) that doesn't fit a single string.
-- `recorded_at` is unix seconds.
--
-- Component keys (initial set; new components MUST land via a new
-- migration that documents the key):
--   schema, embedding_model, embedding_dimension, reranker_model,
--   chunker_version, semantic_text_version,
--   parser_rust, parser_python, parser_java, parser_kotlin

CREATE TABLE index_metadata (
    repo_id TEXT NOT NULL,
    component TEXT NOT NULL,
    version TEXT NOT NULL,
    value_json TEXT NOT NULL DEFAULT '{}',
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, component)
);

-- Backfill: every existing repo gets a `schema=current` row so callers
-- can distinguish "no metadata yet" (old indexes pre-v0.7) from "we
-- ran the migration but the indexer hasn't recorded anything yet".
-- Other component keys are intentionally absent — runtime startup
-- reports them as Unknown rather than guessing what the prior
-- chunker / parser / model was.
INSERT INTO index_metadata (repo_id, component, version, value_json, recorded_at)
  SELECT id, 'schema', '3', '{}', strftime('%s', 'now') FROM repo;
