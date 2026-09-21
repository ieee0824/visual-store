ALTER TABLE schema_migrations RENAME TO schema_migrations_v2;

CREATE TABLE schema_migrations (
    migration_id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_version INTEGER NOT NULL,
    to_version INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    backup_file TEXT NOT NULL,
    CHECK(from_version>=1 AND to_version=from_version+1)
);

INSERT INTO schema_migrations(
    migration_id,from_version,to_version,started_at,completed_at,backup_file
)
SELECT migration_id,from_version,to_version,started_at,completed_at,backup_file
FROM schema_migrations_v2;

DROP TABLE schema_migrations_v2;

CREATE TABLE judgments (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    judgment_id TEXT NOT NULL UNIQUE CHECK(length(judgment_id)=36),
    image_id TEXT NOT NULL REFERENCES images(image_id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(length(CAST(kind AS BLOB)) BETWEEN 1 AND 128),
    producer TEXT NOT NULL CHECK(length(CAST(producer AS BLOB)) BETWEEN 1 AND 128),
    model TEXT CHECK(model IS NULL OR length(CAST(model AS BLOB)) BETWEEN 1 AND 256),
    schema_version INTEGER NOT NULL CHECK(schema_version>=1),
    value_json TEXT NOT NULL CHECK(length(CAST(value_json AS BLOB)) BETWEEN 1 AND 4096),
    probability REAL CHECK(probability IS NULL OR probability BETWEEN 0.0 AND 1.0),
    confidence REAL CHECK(confidence IS NULL OR confidence BETWEEN 0.0 AND 1.0),
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK(length(CAST(metadata_json AS BLOB)) BETWEEN 2 AND 4096),
    created_at TEXT NOT NULL,
    CHECK(length(CAST(value_json AS BLOB))+length(CAST(metadata_json AS BLOB))<=6144)
);

CREATE INDEX judgments_by_image_created
ON judgments(image_id,seq DESC);

CREATE INDEX judgments_by_kind_value
ON judgments(kind,value_json,seq DESC);

CREATE INDEX judgments_by_producer_kind
ON judgments(producer,kind,seq DESC);

CREATE INDEX judgments_by_confidence
ON judgments(confidence,seq DESC);

PRAGMA user_version=3;
