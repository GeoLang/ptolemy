-- the sqlx ledger is _sqlx_migrations, this table is the schema changes applied to one dataset
DO $$ BEGIN
    IF to_regclass('dataset_schema_migrations') IS NULL THEN
        ALTER TABLE schema_migrations RENAME TO dataset_schema_migrations;
        ALTER INDEX idx_schema_migrations_version RENAME TO idx_dataset_schema_migrations_version;
    ELSE
        -- a test database that dropped datasets but kept this table replays 017
        DROP TABLE schema_migrations;
    END IF;
END $$;
