//! Embedded, idempotent migrations: every SPEC 5.2 table keyed by `repo_id` first, plus
//! `audit_log` (PLAN 6.8) and the secondary indexes of `docs/contracts/storage-semantics.md`.
//! The changeset id prefix lookup is a range scan on the `changesets` primary key
//! `(repo_id, changeset_id)`, so it needs no extra index.

/// `(name, sql)` pairs applied in order; every statement is idempotent.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_core_tables",
        r#"
CREATE TABLE IF NOT EXISTS repos (
    repo_id     INT8 PRIMARY KEY,
    name        STRING NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS objects (
    repo_id      INT8 NOT NULL,
    object_id    BYTES NOT NULL,
    kind         INT2 NOT NULL,
    size         INT8 NOT NULL,
    stored_size  INT8 NOT NULL,
    inline_data  BYTES NULL,
    state        STRING NOT NULL DEFAULT 'live',
    created_at   TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (repo_id, object_id)
);
CREATE TABLE IF NOT EXISTS object_uploads (
    repo_id    INT8 NOT NULL,
    object_id  BYTES NOT NULL,
    principal  STRING NOT NULL,
    at         TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (repo_id, object_id, principal)
);
CREATE TABLE IF NOT EXISTS object_paths (
    repo_id          INT8 NOT NULL,
    object_id        BYTES NOT NULL,
    path             BYTES NOT NULL,
    first_changeset  BYTES NOT NULL,
    at               TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (repo_id, object_id, path)
);
CREATE TABLE IF NOT EXISTS changesets (
    repo_id       INT8 NOT NULL,
    changeset_id  BYTES NOT NULL,
    encoded       BYTES NOT NULL,
    generation    INT8 NOT NULL,
    skip          BYTES[] NOT NULL,
    change_id     BYTES NOT NULL,
    created_by    STRING NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (repo_id, changeset_id)
);
CREATE TABLE IF NOT EXISTS changeset_children (
    repo_id    INT8 NOT NULL,
    parent_id  BYTES NOT NULL,
    child_id   BYTES NOT NULL,
    PRIMARY KEY (repo_id, parent_id, child_id)
);
CREATE TABLE IF NOT EXISTS refs (
    repo_id     INT8 NOT NULL,
    name        STRING NOT NULL,
    target      BYTES NOT NULL,
    version     INT8 NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (repo_id, name)
);
CREATE TABLE IF NOT EXISTS ref_log (
    repo_id     INT8 NOT NULL,
    name        STRING NOT NULL,
    version     INT8 NOT NULL,
    old_target  BYTES NULL,
    new_target  BYTES NULL,
    principal   STRING NOT NULL,
    at          TIMESTAMPTZ NOT NULL,
    landed      BYTES NOT NULL,
    PRIMARY KEY (repo_id, name, version)
);
CREATE TABLE IF NOT EXISTS landed (
    repo_id           INT8 NOT NULL,
    ref               STRING NOT NULL,
    change_id         BYTES NOT NULL,
    version           INT8 NOT NULL,
    source_changeset  BYTES NOT NULL,
    result_changeset  BYTES NOT NULL,
    PRIMARY KEY (repo_id, ref, change_id)
);
CREATE TABLE IF NOT EXISTS derived (
    repo_id        INT8 NOT NULL,
    type           STRING NOT NULL,
    type_version   INT8 NOT NULL,
    key            BYTES NOT NULL,
    value          BYTES NULL,
    storage_key    STRING NULL,
    source_object  BYTES NULL,
    at             TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (repo_id, type, type_version, key)
);
CREATE TABLE IF NOT EXISTS derive_leases (
    repo_id       INT8 NOT NULL,
    type          STRING NOT NULL,
    type_version  INT8 NOT NULL,
    key           BYTES NOT NULL,
    holder        STRING NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (repo_id, type, type_version, key)
);
CREATE TABLE IF NOT EXISTS redactions (
    repo_id      INT8 NOT NULL,
    object_id    BYTES NOT NULL,
    state        STRING NOT NULL,
    reason       STRING NOT NULL,
    actor        STRING NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    purge_after  TIMESTAMPTZ NOT NULL,
    epoch        INT8 NOT NULL,
    PRIMARY KEY (repo_id, object_id)
);
CREATE TABLE IF NOT EXISTS counters (
    repo_id  INT8 NOT NULL,
    name     STRING NOT NULL,
    value    INT8 NOT NULL,
    PRIMARY KEY (repo_id, name)
);
CREATE TABLE IF NOT EXISTS audit_log (
    repo_id  INT8 NOT NULL,
    seq      INT8 NOT NULL DEFAULT unique_rowid(),
    at       TIMESTAMPTZ NOT NULL,
    actor    STRING NOT NULL,
    action   STRING NOT NULL,
    target   STRING NOT NULL,
    reason   STRING NOT NULL,
    detail   STRING NOT NULL,
    PRIMARY KEY (repo_id, seq)
);
"#,
    ),
    (
        "0002_secondary_indexes",
        r#"
CREATE INDEX IF NOT EXISTS derived_by_source ON derived (repo_id, source_object) WHERE source_object IS NOT NULL;
CREATE INDEX IF NOT EXISTS redactions_by_epoch ON redactions (repo_id, epoch);
CREATE INDEX IF NOT EXISTS redactions_by_purge_after ON redactions (repo_id, purge_after) WHERE state = 'quarantined';
"#,
    ),
];
