-- Plan 27: chunk-level embed cache.
--
-- Maps (content_hash, embed_model) -> 384-float embedding vector,
-- stored using the same vec_codec as vec_hunk. The embed stage
-- consults this before calling the embedder so identical chunk
-- content is embedded exactly once per (model) value.
--
-- content_hash is sha256-hex (64 chars) when populated by EmbedMode
-- in {Semantic, Diff}; the column is plain TEXT to stay agnostic to
-- the hash function in case a future RFC swaps it.
CREATE TABLE chunk_embed_cache (
  content_hash TEXT NOT NULL,
  embed_model  TEXT NOT NULL,
  diff_emb     BLOB NOT NULL,
  PRIMARY KEY (content_hash, embed_model)
) WITHOUT ROWID;
