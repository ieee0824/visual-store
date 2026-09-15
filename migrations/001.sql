CREATE TABLE store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE blobs (
    sha256 TEXT PRIMARY KEY CHECK(length(sha256)=64),
    relative_path TEXT NOT NULL UNIQUE,
    byte_length INTEGER NOT NULL CHECK(byte_length>0),
    media_type TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE images (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    image_id TEXT NOT NULL UNIQUE,
    run TEXT, created_at TEXT NOT NULL, captured_at TEXT, label TEXT, note TEXT,
    tags_json TEXT NOT NULL DEFAULT '[]',
    width INTEGER NOT NULL CHECK(width>0), height INTEGER NOT NULL CHECK(height>0),
    bit_depth INTEGER NOT NULL, color_type INTEGER NOT NULL,
    source_sha256 TEXT NOT NULL, source_byte_length INTEGER NOT NULL CHECK(source_byte_length>0),
    stored_blob_sha256 TEXT NOT NULL REFERENCES blobs(sha256),
    source_blob_sha256 TEXT REFERENCES blobs(sha256),
    scanline_sha256 TEXT NOT NULL, non_idat_sha256 TEXT NOT NULL, pixel_sha256 TEXT NOT NULL,
    encoding_version TEXT NOT NULL, compression_level INTEGER NOT NULL,
    compression_applied INTEGER NOT NULL CHECK(compression_applied IN (0,1)),
    operation_id TEXT UNIQUE, operation_fingerprint TEXT,
    validation_limits_json TEXT NOT NULL
);
CREATE INDEX images_by_run_seq ON images(run,seq DESC);
CREATE INDEX images_by_stored_blob ON images(stored_blob_sha256);
CREATE INDEX images_by_source_blob ON images(source_blob_sha256);
PRAGMA user_version=1;
