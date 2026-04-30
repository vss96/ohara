-- ohara index schema, version 1.

CREATE TABLE repo (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    first_commit_sha TEXT NOT NULL,
    last_indexed_commit TEXT,
    indexed_at TEXT,
    schema_version INTEGER NOT NULL
);

CREATE TABLE commit_record (
    sha TEXT PRIMARY KEY,
    parent_sha TEXT,
    is_merge INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    author TEXT,
    message TEXT NOT NULL
);
CREATE INDEX idx_commit_ts ON commit_record (ts);

CREATE TABLE file_path (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    language TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    UNIQUE(path)
);

CREATE TABLE symbol (
    id INTEGER PRIMARY KEY,
    file_path_id INTEGER NOT NULL REFERENCES file_path(id),
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT,
    span_start INTEGER NOT NULL,
    span_end INTEGER NOT NULL,
    blob_sha TEXT NOT NULL,
    source_text TEXT NOT NULL
);
CREATE INDEX idx_symbol_file ON symbol (file_path_id);

CREATE TABLE hunk (
    id INTEGER PRIMARY KEY,
    commit_sha TEXT NOT NULL REFERENCES commit_record(sha),
    file_path_id INTEGER NOT NULL REFERENCES file_path(id),
    change_kind TEXT NOT NULL,
    diff_text TEXT NOT NULL
);
CREATE INDEX idx_hunk_file_commit ON hunk (file_path_id, commit_sha);
CREATE INDEX idx_hunk_commit ON hunk (commit_sha);

CREATE TABLE blob_cache (
    blob_sha TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    embedded_at INTEGER NOT NULL,
    PRIMARY KEY (blob_sha, embedding_model)
);

CREATE VIRTUAL TABLE vec_hunk USING vec0(hunk_id INTEGER PRIMARY KEY, diff_emb FLOAT[384]);
CREATE VIRTUAL TABLE vec_commit USING vec0(commit_sha TEXT PRIMARY KEY, message_emb FLOAT[384]);
CREATE VIRTUAL TABLE vec_symbol USING vec0(symbol_id INTEGER PRIMARY KEY, source_emb FLOAT[384]);

CREATE VIRTUAL TABLE fts_commit USING fts5(sha UNINDEXED, message);
CREATE VIRTUAL TABLE fts_symbol USING fts5(symbol_id UNINDEXED, qualified_name, source_text);
