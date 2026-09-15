CREATE TABLE blobs (
    sha256 TEXT PRIMARY KEY CHECK(length(sha256)=64),
    relative_path TEXT NOT NULL UNIQUE,
    byte_length INTEGER NOT NULL CHECK(byte_length>0),
    media_type TEXT NOT NULL,
    object_kind TEXT NOT NULL CHECK(object_kind IN ('png','vp9_bitstream','png_reconstruction')),
    created_at TEXT NOT NULL
);

CREATE TABLE images (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    image_id TEXT NOT NULL UNIQUE,
    run TEXT CHECK(run IS NULL OR (length(CAST(run AS BLOB)) BETWEEN 1 AND 128)),
    stream TEXT CHECK(stream IS NULL OR (length(CAST(stream AS BLOB)) BETWEEN 1 AND 128)),
    frame_no INTEGER CHECK(frame_no IS NULL OR frame_no>=0),
    created_at TEXT NOT NULL,
    captured_at TEXT,
    label TEXT,
    note TEXT,
    tags_json TEXT NOT NULL DEFAULT '[]',
    width INTEGER NOT NULL CHECK(width>0),
    height INTEGER NOT NULL CHECK(height>0),
    bit_depth INTEGER NOT NULL,
    color_type INTEGER NOT NULL,
    source_sha256 TEXT NOT NULL CHECK(length(source_sha256)=64),
    source_byte_length INTEGER NOT NULL CHECK(source_byte_length>0),
    source_blob_sha256 TEXT REFERENCES blobs(sha256),
    scanline_sha256 TEXT NOT NULL CHECK(length(scanline_sha256)=64),
    non_idat_sha256 TEXT NOT NULL CHECK(length(non_idat_sha256)=64),
    pixel_sha256 TEXT NOT NULL CHECK(length(pixel_sha256)=64),
    operation_id TEXT UNIQUE,
    operation_fingerprint TEXT CHECK(operation_fingerprint IS NULL OR length(operation_fingerprint)=64),
    operation_fingerprint_version INTEGER CHECK(operation_fingerprint_version IS NULL OR operation_fingerprint_version IN (1,2)),
    validation_limits_json TEXT NOT NULL,
    CHECK(
        (run IS NULL AND stream IS NULL AND frame_no IS NULL) OR
        (run IS NOT NULL AND stream IS NOT NULL AND frame_no IS NOT NULL)
    ),
    CHECK(
        (operation_fingerprint IS NULL AND operation_fingerprint_version IS NULL AND operation_id IS NULL) OR
        (operation_fingerprint IS NOT NULL AND operation_fingerprint_version IS NOT NULL)
    ),
    UNIQUE(run,stream,frame_no)
);

CREATE TABLE segments (
    segment_id TEXT PRIMARY KEY,
    codec TEXT NOT NULL,
    codec_descriptor_json TEXT NOT NULL,
    width INTEGER NOT NULL CHECK(width>0),
    height INTEGER NOT NULL CHECK(height>0),
    bit_depth INTEGER NOT NULL CHECK(bit_depth>0),
    pixel_layout TEXT NOT NULL,
    frame_count INTEGER NOT NULL CHECK(frame_count BETWEEN 2 AND 128),
    color_blob_sha256 TEXT NOT NULL REFERENCES blobs(sha256),
    alpha_blob_sha256 TEXT REFERENCES blobs(sha256),
    created_at TEXT NOT NULL
);

CREATE TABLE frame_locations (
    image_id TEXT PRIMARY KEY REFERENCES images(image_id) ON DELETE CASCADE,
    segment_id TEXT NOT NULL REFERENCES segments(segment_id),
    frame_index INTEGER NOT NULL CHECK(frame_index>=0),
    decode_start_index INTEGER NOT NULL CHECK(decode_start_index>=0 AND decode_start_index<=frame_index),
    UNIQUE(image_id,segment_id),
    UNIQUE(segment_id,frame_index)
);

CREATE TRIGGER frame_locations_bounds_insert
BEFORE INSERT ON frame_locations
WHEN NEW.frame_index >= (SELECT frame_count FROM segments WHERE segment_id=NEW.segment_id)
  OR NEW.decode_start_index >= (SELECT frame_count FROM segments WHERE segment_id=NEW.segment_id)
BEGIN
    SELECT RAISE(ABORT,'frame location exceeds segment bounds');
END;

CREATE TRIGGER frame_locations_bounds_update
BEFORE UPDATE OF segment_id,frame_index,decode_start_index ON frame_locations
WHEN NEW.frame_index >= (SELECT frame_count FROM segments WHERE segment_id=NEW.segment_id)
  OR NEW.decode_start_index >= (SELECT frame_count FROM segments WHERE segment_id=NEW.segment_id)
BEGIN
    SELECT RAISE(ABORT,'frame location exceeds segment bounds');
END;

CREATE TRIGGER segments_immutable
BEFORE UPDATE ON segments
BEGIN
    SELECT RAISE(ABORT,'committed segments are immutable');
END;

CREATE TABLE representations (
    image_id TEXT PRIMARY KEY REFERENCES images(image_id) ON DELETE CASCADE,
    representation_version INTEGER NOT NULL CHECK(representation_version>0),
    representation_kind TEXT NOT NULL CHECK(representation_kind IN ('png','vp9_segment')),
    png_blob_sha256 TEXT REFERENCES blobs(sha256),
    segment_id TEXT,
    encoding_version TEXT NOT NULL,
    compression_level INTEGER CHECK(compression_level IS NULL OR compression_level BETWEEN 0 AND 9),
    compression_applied INTEGER CHECK(compression_applied IS NULL OR compression_applied IN (0,1)),
    created_at TEXT NOT NULL,
    verified_at TEXT NOT NULL,
    CHECK(
        (representation_kind='png' AND png_blob_sha256 IS NOT NULL AND segment_id IS NULL
            AND compression_level IS NOT NULL AND compression_applied IS NOT NULL) OR
        (representation_kind='vp9_segment' AND png_blob_sha256 IS NULL AND segment_id IS NOT NULL
            AND compression_level IS NULL AND compression_applied IS NULL)
    ),
    FOREIGN KEY(image_id,segment_id) REFERENCES frame_locations(image_id,segment_id)
);

CREATE TABLE png_reconstruction (
    image_id TEXT PRIMARY KEY REFERENCES images(image_id) ON DELETE CASCADE,
    reconstruction_version INTEGER NOT NULL CHECK(reconstruction_version>0),
    descriptor_blob_sha256 TEXT NOT NULL REFERENCES blobs(sha256),
    descriptor_byte_length INTEGER NOT NULL CHECK(descriptor_byte_length>0),
    created_at TEXT NOT NULL
);

CREATE TABLE retired_representations (
    retired_id INTEGER PRIMARY KEY AUTOINCREMENT,
    image_id TEXT NOT NULL REFERENCES images(image_id) ON DELETE CASCADE,
    representation_version INTEGER NOT NULL CHECK(representation_version>0),
    representation_kind TEXT NOT NULL CHECK(representation_kind IN ('png','vp9_segment')),
    png_blob_sha256 TEXT REFERENCES blobs(sha256),
    segment_id TEXT REFERENCES segments(segment_id),
    encoding_version TEXT NOT NULL,
    retired_at TEXT NOT NULL,
    replacement_version INTEGER NOT NULL CHECK(replacement_version>representation_version),
    prune_state TEXT NOT NULL CHECK(prune_state IN ('retained','pending','deleted')),
    CHECK(
        (representation_kind='png' AND png_blob_sha256 IS NOT NULL AND segment_id IS NULL) OR
        (representation_kind='vp9_segment' AND png_blob_sha256 IS NULL AND segment_id IS NOT NULL)
    ),
    UNIQUE(image_id,representation_version)
);

CREATE TABLE schema_migrations (
    migration_id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_version INTEGER NOT NULL,
    to_version INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    backup_file TEXT NOT NULL,
    CHECK(from_version=1 AND to_version=2)
);

CREATE INDEX images_by_run_seq ON images(run,seq DESC);
CREATE INDEX images_by_stream_frame ON images(run,stream,frame_no);
CREATE INDEX images_by_source_blob ON images(source_blob_sha256);
CREATE INDEX representations_by_png_blob ON representations(png_blob_sha256);
CREATE INDEX representations_by_segment ON representations(segment_id);
CREATE INDEX frame_locations_by_segment ON frame_locations(segment_id,frame_index);
CREATE INDEX reconstruction_by_blob ON png_reconstruction(descriptor_blob_sha256);
CREATE INDEX retired_by_png_blob ON retired_representations(png_blob_sha256,prune_state);
CREATE INDEX retired_by_segment ON retired_representations(segment_id,prune_state);

PRAGMA user_version=2;
