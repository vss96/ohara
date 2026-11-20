-- Issue #141: `blob_cache` (V1) never had a production writer. Embed
-- dedup is owned by the plan-27 `chunk_embed_cache` (V5).
DROP TABLE IF EXISTS blob_cache;
