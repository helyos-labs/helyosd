-- Recreate deployments table with ON DELETE CASCADE on the project FK
-- so that deleting a project automatically removes its deployments (and pods cascade from deployments).
--
-- WHY foreign_keys=OFF is required here:
-- SQLite does not support ALTER TABLE ... ADD CONSTRAINT. The only way to
-- change a foreign-key definition is the 12-step "rename-recreate" procedure
-- documented at https://www.sqlite.org/lang_altertable.html#otheralter.
-- During this procedure SQLite requires foreign_keys=OFF because:
--   1. DROP TABLE on the old table would trigger FK violations from the
--      pods table that references deployments(id).
--   2. The rename of deployments_new -> deployments must happen without
--      FK enforcement checking the intermediate state.
--
-- This migration file uses the .no_tx.sql extension (sqlx runs it without
-- an automatic transaction) so we wrap the critical section in our own
-- SAVEPOINT to ensure atomicity — if any step fails the entire set of
-- changes is rolled back and foreign_keys is re-enabled.

PRAGMA foreign_keys=OFF;

SAVEPOINT cascade_migration;

CREATE TABLE deployments_new (
    id          TEXT PRIMARY KEY,
    project     TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    spec_json   TEXT NOT NULL,
    status      TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE(project, name)
);

INSERT INTO deployments_new SELECT id, project, name, spec_json, status, created_at, updated_at FROM deployments;

DROP TABLE deployments;

ALTER TABLE deployments_new RENAME TO deployments;

RELEASE cascade_migration;

PRAGMA foreign_keys=ON;

-- Verify FK integrity after re-enabling constraints.
PRAGMA foreign_key_check(deployments);
