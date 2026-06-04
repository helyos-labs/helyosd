-- Drop the unused `secrets` table from the state database (helyos.db).
--
-- This table was created by the initial schema but never read or written:
-- application secrets live in a separate, encrypted store (secrets.db, managed
-- by the rusqlite-backed `EncryptedSecretStore` with its own schema). The
-- `secrets` table here is dead schema and only widens the surface area of the
-- state DB, so we remove it. No other table references it (no inbound foreign
-- keys), and it is always empty in practice, making this drop safe for
-- existing deployments.
DROP TABLE IF EXISTS secrets;
