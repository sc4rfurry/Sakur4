//! SQLite schema and migrations for the Memory Fabric (PRD §data_model).
//!
//! Two architectural invariants of Sakur4 are enforced *by the database*, not
//! merely by convention, so that no future code path can quietly violate them:
//!
//! * **FR-1 — append-only Episodic Stream.** `UPDATE` and `DELETE` on
//!   `episodic_stream` are blocked by triggers. Corrections are new rows that
//!   reference the corrected row.
//! * **FR-3 — mandatory Semantic Atlas anchoring.** `semantic_atlas` has a
//!   `NOT NULL` anchor plus `CHECK` constraints on the anchor type, and the
//!   staleness view resolves the anchor hash *live* rather than trusting a
//!   value written at write time.

use crate::error::Result;
use crate::store::vector::VectorBackend;

/// Pragmas applied to every connection.
///
/// WAL keeps readers unblocked while the harness writes episodes (FR-1's
/// "non-blocking write path"); `synchronous=NORMAL` is the standard WAL
/// durability trade-off and still guarantees no corruption (NFR-5).
pub const CONNECTION_PRAGMAS: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = -16000;
";

/// One forward migration.
pub struct Migration {
    /// Monotonic version; rows in `meta` record the applied high-water mark.
    pub version: i64,
    /// Human-readable name, for logs and the `doctor` command.
    pub name: &'static str,
    /// SQL executed inside a single transaction.
    pub sql: &'static str,
}

/// The ordered migration list. Never edit a shipped migration; append instead.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial_memory_fabric",
    sql: V1,
}];

const V1: &str = r#"
-- ---------------------------------------------------------------------------
-- Bookkeeping
-- ---------------------------------------------------------------------------
CREATE TABLE meta (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL
);

-- A project is one indexed repository + its memory. v1 supports many projects
-- in one store; the CLI/MCP surface defaults to the single "default" project.
CREATE TABLE project (
    project_id   TEXT PRIMARY KEY,
    root_path    TEXT NOT NULL,
    name         TEXT NOT NULL,
    indexed_at   TEXT,
    index_commit TEXT
);

CREATE TABLE session (
    session_id   TEXT PRIMARY KEY,
    project_id   TEXT REFERENCES project(project_id) ON DELETE SET NULL,
    slot_id      TEXT,
    description  TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- C1 · Episodic Stream — raw, append-only, the source of truth (FR-1)
-- ---------------------------------------------------------------------------
CREATE TABLE episodic_stream (
    episode_id   TEXT PRIMARY KEY,
    seq          INTEGER NOT NULL,
    session_id   TEXT NOT NULL,
    slot_id      TEXT,
    role         TEXT NOT NULL,
    content      TEXT NOT NULL,
    tool_name    TEXT,
    token_count  INTEGER NOT NULL,
    created_at   TEXT NOT NULL,
    fold_id      TEXT,
    -- Raw content is retained verbatim here under all circumstances; the
    -- graduated eviction tiers (FR-5) act on *live-context inclusion*, never on
    -- this column, which is why an evicted-then-recalled episode is always
    -- bit-identical to the original.
    eviction_tier      TEXT NOT NULL DEFAULT 'live',
    droppable          INTEGER NOT NULL DEFAULT 0,
    superseded_by      TEXT,
    corrected_episode  TEXT,
    raw_ref            TEXT,
    meta_json          TEXT,
    CHECK (eviction_tier IN ('live','masked','referenced','archived','dropped')),
    CHECK (droppable IN (0,1))
);
CREATE UNIQUE INDEX episodic_stream_seq ON episodic_stream(seq);
CREATE INDEX episodic_stream_session ON episodic_stream(session_id, seq);
CREATE INDEX episodic_stream_fold ON episodic_stream(fold_id) WHERE fold_id IS NOT NULL;
CREATE INDEX episodic_stream_tier ON episodic_stream(eviction_tier);

-- Append-only enforcement, scoped to what "append-only" actually means.
--
-- The invariant is that *recorded content* can never change or disappear. The
-- eviction tier and bookkeeping columns are not recorded content — they are
-- Sakur4's own annotations about how to render a row, and the eviction engine
-- must be able to move them (FR-5). Blocking them would make the tier system
-- unimplementable, while blocking `content` is exactly what makes FR-5's
-- round-trip-integrity criterion true by construction.
CREATE TRIGGER episodic_stream_content_immutable
BEFORE UPDATE OF content, role, tool_name, seq, session_id, episode_id ON episodic_stream
BEGIN
    SELECT RAISE(ABORT, 'episodic_stream is append-only: recorded content may not change; corrections must be new rows referencing the corrected one');
END;

CREATE TRIGGER episodic_stream_no_delete
BEFORE DELETE ON episodic_stream
BEGIN
    SELECT RAISE(ABORT, 'episodic_stream is append-only: rows may never be deleted');
END;

-- ---------------------------------------------------------------------------
-- C5 · Lexical index over the stream (FR-12's BM25 retriever)
--
-- A contentless external-content FTS5 table: the index stores only tokens and
-- rowids, and `episodic_stream.content` remains the single copy of the text.
-- The triggers keep the index in step; because the stream is append-only, they
-- are insert-only by construction, which is also why the index can never drift
-- from the source.
--
-- Note that FTS5 exposes the underlying rowid, *not* arbitrary columns of the
-- content table, which is why recall joins on `rowid` rather than `episode_id`.
-- ---------------------------------------------------------------------------
CREATE VIRTUAL TABLE episodic_fts USING fts5(
    content,
    content='episodic_stream',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER episodic_stream_fts_insert
AFTER INSERT ON episodic_stream
BEGIN
    INSERT INTO episodic_fts(rowid, content) VALUES (new.rowid, new.content);
END;

-- ---------------------------------------------------------------------------
-- C1 · Symbolic Ledger — deterministic, parser-only facts (FR-2)
--
-- There is deliberately no column an LLM-written value could land in, and the
-- write path in `memory::symbolic` is the only constructor of `SymbolicFact`
-- values. `source` is constrained to the closed set of deterministic extractors.
-- ---------------------------------------------------------------------------
CREATE TABLE symbolic_fact (
    fact_id        TEXT PRIMARY KEY,
    kind           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    file_path      TEXT,
    line_start     INTEGER,
    line_end       INTEGER,
    signature      TEXT,
    ast_hash       TEXT NOT NULL,
    source         TEXT NOT NULL,
    project_id     TEXT,
    parent_name    TEXT,
    body           TEXT,
    updated_at     TEXT NOT NULL,
    CHECK (kind IN ('function','method','class','struct','enum','trait','interface','type',
                    'constant','module','import','export','tool_output_field','tool_exit_code',
                    'http_header','json_field','csv_column','diff_hunk','regex_match')),
    CHECK (source <> '')
);
-- The natural key is (project, qualified name, kind, file), with NULLs folded to
-- the empty string.
--
-- # Why the COALESCE is load-bearing
--
-- SQLite treats NULLs as *distinct* in a UNIQUE index, so a fact with no project
-- (or no file) would never collide with an identical fact — and an incremental
-- re-index would insert a duplicate row instead of refreshing the existing one.
-- That failure is invisible in the read path (the newest row still answers
-- correctly) and catastrophic for staleness, because a summary's anchor keeps
-- pointing at the stale row. Folding NULL to '' where the column is absent makes
-- the natural key actually behave as one.
CREATE UNIQUE INDEX symbolic_fact_identity
    ON symbolic_fact(COALESCE(project_id, ''), qualified_name, kind,
                     COALESCE(file_path, ''));
CREATE INDEX symbolic_fact_file ON symbolic_fact(file_path);
CREATE INDEX symbolic_fact_name ON symbolic_fact(qualified_name);
CREATE INDEX symbolic_fact_kind ON symbolic_fact(kind);

-- ---------------------------------------------------------------------------
-- C1 · Anchor Set — pinned constraints, immune to every eviction tier (FR-4)
-- ---------------------------------------------------------------------------
CREATE TABLE anchor_set (
    anchor_id   TEXT PRIMARY KEY,
    content     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    session_id  TEXT,
    project_id  TEXT,
    pinned_by   TEXT NOT NULL DEFAULT 'user',
    created_at  TEXT NOT NULL,
    CHECK (kind IN ('user_correction','safety_constraint','task_contract')),
    CHECK (content <> '')
);
CREATE INDEX anchor_set_session ON anchor_set(session_id);

-- ---------------------------------------------------------------------------
-- C1 · Semantic Atlas — LLM-derived interpretation, mandatorily anchored (FR-3)
-- ---------------------------------------------------------------------------
CREATE TABLE semantic_atlas (
    atlas_id             TEXT PRIMARY KEY,
    content              TEXT NOT NULL,
    -- anchor_id references ONE logical anchor which may live in either track;
    -- the pairing table below carries the full anchor set when an entry was
    -- derived from several episodes.
    anchor_type          TEXT NOT NULL,
    anchor_id            TEXT NOT NULL,
    anchor_hash_at_write TEXT NOT NULL,
    model                TEXT,
    project_id           TEXT,
    session_id           TEXT,
    created_at           TEXT NOT NULL,
    regenerated_at       TEXT,
    CHECK (anchor_type IN ('symbolic_fact','episodic_stream')),
    CHECK (content <> '')
);
CREATE INDEX semantic_atlas_anchor ON semantic_atlas(anchor_type, anchor_id);
CREATE INDEX semantic_atlas_project ON semantic_atlas(project_id);

CREATE TABLE semantic_anchor_link (
    atlas_id  TEXT NOT NULL REFERENCES semantic_atlas(atlas_id) ON DELETE CASCADE,
    anchor_type TEXT NOT NULL,
    anchor_id   TEXT NOT NULL,
    PRIMARY KEY (atlas_id, anchor_type, anchor_id),
    CHECK (anchor_type IN ('symbolic_fact','episodic_stream'))
);

-- The Atlas gets its own lexical index, declared here because an FTS5
-- external-content table names its content table and SQLite resolves that name
-- when the virtual table is created — declaring it before `semantic_atlas`
-- exists makes every query fail with "no such table: main.semantic_atlas".
--
-- Unlike the stream, Atlas rows *are* refreshed (the Idle Consolidator rewrites
-- stale summaries), so this index needs an update trigger as well as insert and
-- delete.
CREATE VIRTUAL TABLE semantic_fts USING fts5(
    content,
    content='semantic_atlas',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER semantic_atlas_fts_insert
AFTER INSERT ON semantic_atlas
BEGIN
    INSERT INTO semantic_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER semantic_atlas_fts_update
AFTER UPDATE OF content ON semantic_atlas
BEGIN
    INSERT INTO semantic_fts(semantic_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
    INSERT INTO semantic_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER semantic_atlas_fts_delete
AFTER DELETE ON semantic_atlas
BEGIN
    INSERT INTO semantic_fts(semantic_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
END;

-- Staleness is computed at read time against the *current* anchor hash, so a
-- drifted summary is caught even if nothing ran to refresh a flag (INN-4).
CREATE VIEW semantic_atlas_staleness AS
SELECT
    sa.atlas_id,
    sa.project_id,
    sa.anchor_type,
    sa.anchor_id,
    sa.anchor_hash_at_write,
    CASE sa.anchor_type
        WHEN 'symbolic_fact' THEN (SELECT sf.ast_hash FROM symbolic_fact sf WHERE sf.fact_id = sa.anchor_id)
        ELSE (SELECT printf('%016x', e.seq) FROM episodic_stream e WHERE e.episode_id = sa.anchor_id)
    END AS current_anchor_hash
FROM semantic_atlas sa;

-- ---------------------------------------------------------------------------
-- C1 · Dependency Graph — one table for code and memory edges (PRD §data_model)
-- ---------------------------------------------------------------------------
CREATE TABLE dependency_graph_edge (
    src_type  TEXT NOT NULL,
    src_id    TEXT NOT NULL,
    dst_type  TEXT NOT NULL,
    dst_id    TEXT NOT NULL,
    edge_kind TEXT NOT NULL,
    weight    REAL NOT NULL DEFAULT 1.0,
    created_at TEXT NOT NULL,
    PRIMARY KEY (src_type, src_id, dst_type, dst_id, edge_kind),
    CHECK (edge_kind IN ('calls','imports','derived_from','depends_on','supersedes','corrects','folded_from'))
);
CREATE INDEX dep_edge_src ON dependency_graph_edge(src_type, src_id);
CREATE INDEX dep_edge_dst ON dependency_graph_edge(dst_type, dst_id);
CREATE INDEX dep_edge_kind ON dependency_graph_edge(edge_kind);

-- ---------------------------------------------------------------------------
-- C2 · Folds — agent-directed sub-context isolation (FR-6)
-- ---------------------------------------------------------------------------
CREATE TABLE folds (
    fold_id                TEXT PRIMARY KEY,
    session_id             TEXT NOT NULL,
    slot_id                TEXT,
    description            TEXT NOT NULL,
    goal                   TEXT NOT NULL,
    status                 TEXT NOT NULL,
    opened_checkpoint_id   TEXT,
    closed_checkpoint_id   TEXT,
    opened_token_position  INTEGER,
    result_summary         TEXT,
    tokens_at_open         INTEGER,
    tokens_reclaimed       INTEGER,
    created_at             TEXT NOT NULL,
    closed_at              TEXT,
    CHECK (status IN ('open','closed')),
    CHECK (description <> '')
);
CREATE INDEX folds_session ON folds(session_id, status);

-- ---------------------------------------------------------------------------
-- C3 · Cache-Coherence Layer — mirror of llama.cpp slot/checkpoint state
-- ---------------------------------------------------------------------------
CREATE TABLE cache_checkpoint (
    checkpoint_id  TEXT PRIMARY KEY,
    slot_id        TEXT NOT NULL,
    session_id     TEXT,
    token_position INTEGER NOT NULL,
    kind           TEXT NOT NULL,
    file_path      TEXT,
    size_bytes     INTEGER,
    n_saved        INTEGER,
    created_at     TEXT NOT NULL,
    backend        TEXT NOT NULL DEFAULT 'unknown',
    CHECK (kind IN ('internal_checkpoint','slot_save_file','fold_marker','pre_rewrite'))
);
CREATE INDEX cache_checkpoint_slot ON cache_checkpoint(slot_id, token_position DESC);
CREATE INDEX cache_checkpoint_session ON cache_checkpoint(session_id);

CREATE TABLE slot_state (
    slot_id          TEXT PRIMARY KEY,
    session_id       TEXT,
    backend          TEXT NOT NULL DEFAULT 'unknown',
    n_ctx            INTEGER,
    n_past           INTEGER,
    prompt_tokens    INTEGER NOT NULL DEFAULT 0,
    cache_status     TEXT NOT NULL DEFAULT 'unknown',
    -- Opaque per-slot memory written by the Cache-Coherence Layer: the hash of
    -- the context prefix the slot is believed to hold, followed by the
    -- serialised BoundaryPlan that produced it. Storing it here (rather than
    -- recomputing) is what lets `observe_prompt` judge reuse without re-reading
    -- the whole session.
    cache_detail     TEXT,
    last_prompt_eval_ms INTEGER,
    last_seen_at     TEXT NOT NULL,
    CHECK (cache_status IN ('unknown','cold','full-re-prefill','partial-reuse','warm-restored'))
);

-- ---------------------------------------------------------------------------
-- C8 · Context Ledger Receipt — per-turn accounting (FR-15)
-- ---------------------------------------------------------------------------
CREATE TABLE receipt (
    receipt_id      TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    slot_id         TEXT,
    turn            INTEGER NOT NULL,
    total_tokens    INTEGER NOT NULL,
    breakdown_json  TEXT NOT NULL,
    cache_status    TEXT NOT NULL,
    cache_detail    TEXT,
    prompt_eval_ms  INTEGER,
    eviction_json   TEXT,
    created_at      TEXT NOT NULL
);
CREATE INDEX receipt_session ON receipt(session_id, turn DESC);

-- ---------------------------------------------------------------------------
-- NFR-13 · structured audit log: every eviction / fold / coherence decision
-- ---------------------------------------------------------------------------
CREATE TABLE coherence_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          TEXT NOT NULL,
    session_id  TEXT,
    slot_id     TEXT,
    kind        TEXT NOT NULL,
    detail_json TEXT NOT NULL
);
CREATE INDEX coherence_log_session ON coherence_log(session_id, id DESC);

-- ---------------------------------------------------------------------------
-- C4 · Repo Cortex — file index + graph
-- ---------------------------------------------------------------------------
CREATE TABLE repo_file (
    project_id   TEXT NOT NULL,
    rel_path     TEXT NOT NULL,
    language     TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    symbols      INTEGER NOT NULL DEFAULT 0,
    parsed_at    TEXT NOT NULL,
    PRIMARY KEY (project_id, rel_path)
);

-- ---------------------------------------------------------------------------
-- C5 · Vectors — dense embeddings.
--
-- `vector_backend` records which engine actually serves similarity search on
-- this machine ('sqlite-vec' when a loadable extension was found, otherwise the
-- built-in 'bruteforce' scan). The MCP contract is identical either way.
-- ---------------------------------------------------------------------------
CREATE TABLE vectors (
    embedding_id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_table TEXT NOT NULL,
    source_id    TEXT NOT NULL,
    dim          INTEGER NOT NULL,
    model        TEXT NOT NULL,
    norm         REAL NOT NULL,
    vec          BLOB NOT NULL,
    created_at   TEXT NOT NULL,
    UNIQUE (source_table, source_id, model)
);
CREATE INDEX vectors_source ON vectors(source_table, source_id);

-- ---------------------------------------------------------------------------
-- C8 · Provider-reported usage — prompt-cache accounting over cloud providers
--
-- A local llama.cpp slot reports its cache state through its own API. A hosted
-- provider reports it in the completion response instead: how many prompt tokens
-- were served from its prompt cache, how many were written to it, and how many
-- were billed fresh. Sakur4 does not call a provider itself (NG1 — it is a
-- subsystem, not a harness), so the harness pushes those numbers here and the
-- receipt accounts for them.
--
-- This is what makes the Cache Ledger meaningful off llama.cpp. The failure it
-- catches is the same one FR-7 exists for: a compaction that rewrites history
-- invalidates the provider's cached prefix, and the next turn pays full price for
-- tokens that were previously discounted. On a hosted provider that is a *bill*,
-- not just a stall, which makes it worth measuring even when there is no KV slot
-- to align to.
-- ---------------------------------------------------------------------------
CREATE TABLE provider_usage (
    usage_id       TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL,
    slot_id        TEXT,
    turn           INTEGER NOT NULL,
    prompt_tokens  INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    reasoning_tokens   INTEGER,
    provider       TEXT,
    model          TEXT,
    created_at     TEXT NOT NULL
);
CREATE INDEX provider_usage_session ON provider_usage(session_id, turn DESC);

-- ---------------------------------------------------------------------------
-- Per-language parse tallies, so `doctor` can show what the index covers.
-- ---------------------------------------------------------------------------
CREATE VIEW repo_language_summary AS
SELECT project_id, language, COUNT(*) AS files, SUM(symbols) AS symbols
FROM repo_file GROUP BY project_id, language;

INSERT INTO meta(key, value) VALUES
    ('schema_version', '1'),
    ('created_by', 'sakur4d');
"#;

/// Apply every migration whose version is above the stored high-water mark.
///
/// Idempotent: re-running on a current store is a no-op. Each migration runs in
/// its own transaction, so an interrupted upgrade leaves the store at the last
/// fully-applied version rather than half-migrated (NFR-5).
pub fn migrate(conn: &mut rusqlite::Connection) -> Result<(i64, Vec<&'static str>)> {
    conn.execute_batch(CONNECTION_PRAGMAS)?;

    let has_meta: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);

    let current: i64 = if has_meta {
        conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
    } else {
        0
    };

    let mut applied = Vec::new();
    for migration in MIGRATIONS {
        if migration.version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [migration.version.to_string()],
        )?;
        tx.commit()?;
        applied.push(migration.name);
    }

    let final_version: i64 = conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
        [],
        |r| r.get(0),
    )?;
    Ok((final_version, applied))
}

/// Record which similarity backend this store is using, for observability.
pub fn record_vector_backend(conn: &rusqlite::Connection, backend: VectorBackend) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES('vector_backend', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [backend.as_str()],
    )?;
    Ok(())
}
