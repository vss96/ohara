-- harness.sqlite: outcome-harness task log. Separate database and separate
-- migration set from the per-repo index; append-only.
CREATE TABLE task (
    id            TEXT PRIMARY KEY,
    outcome       TEXT NOT NULL,
    workflow      TEXT NOT NULL,
    repo_root     TEXT NOT NULL,
    base_sha      TEXT NOT NULL,
    branch        TEXT NOT NULL,
    state         TEXT NOT NULL,
    deadline_ms   INTEGER NOT NULL,
    cost_ceiling  INTEGER,
    cost_spent    INTEGER NOT NULL,
    contract_hash TEXT,
    heartbeat_ms  INTEGER,
    round         INTEGER NOT NULL,
    created_ms    INTEGER NOT NULL,
    finished_ms   INTEGER,
    detail        TEXT
);

CREATE TABLE step (
    task_id       TEXT NOT NULL,
    round         INTEGER NOT NULL,
    seq           INTEGER NOT NULL,
    role          TEXT NOT NULL,
    runtime       TEXT NOT NULL,
    model         TEXT,
    state         TEXT NOT NULL,
    started_ms    INTEGER NOT NULL,
    finished_ms   INTEGER,
    head_sha      TEXT,
    cost          INTEGER,
    failover_from TEXT,
    transcript    TEXT NOT NULL,
    summary       TEXT,
    PRIMARY KEY (task_id, round, seq)
);

CREATE TABLE decision (
    id             TEXT PRIMARY KEY,
    task_id        TEXT NOT NULL,
    round          INTEGER NOT NULL,
    parent_ids     TEXT NOT NULL,
    question       TEXT NOT NULL,
    options        TEXT NOT NULL,
    chosen         TEXT NOT NULL,
    runner_up      TEXT,
    assumptions    TEXT NOT NULL,
    cost_if_wrong  TEXT NOT NULL,
    checkpoint_sha TEXT,
    evidence       TEXT NOT NULL,
    state          TEXT NOT NULL,
    rejected_reason TEXT,
    human_verdict  TEXT,
    verdict_note   TEXT
);

CREATE INDEX decision_task_id ON decision(task_id);
