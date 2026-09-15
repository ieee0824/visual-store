INSERT INTO blobs(sha256,relative_path,byte_length,media_type,object_kind,created_at)
SELECT sha256,relative_path,byte_length,media_type,'png',created_at
FROM blobs_v1;

INSERT INTO images(
    seq,image_id,run,stream,frame_no,created_at,captured_at,label,note,tags_json,
    width,height,bit_depth,color_type,source_sha256,source_byte_length,
    source_blob_sha256,scanline_sha256,non_idat_sha256,pixel_sha256,
    operation_id,operation_fingerprint,operation_fingerprint_version,
    validation_limits_json
)
SELECT
    seq,image_id,run,
    CASE WHEN run IS NULL THEN NULL ELSE 'default' END,
    CASE WHEN run IS NULL THEN NULL ELSE
        ROW_NUMBER() OVER (PARTITION BY run ORDER BY seq)-1
    END,
    created_at,captured_at,label,note,tags_json,width,height,bit_depth,color_type,
    source_sha256,source_byte_length,source_blob_sha256,scanline_sha256,
    non_idat_sha256,pixel_sha256,operation_id,operation_fingerprint,
    CASE WHEN operation_fingerprint IS NULL THEN NULL ELSE 1 END,
    validation_limits_json
FROM images_v1
ORDER BY seq;

INSERT INTO representations(
    image_id,representation_version,representation_kind,png_blob_sha256,
    segment_id,encoding_version,compression_level,compression_applied,
    created_at,verified_at
)
SELECT image_id,1,'png',stored_blob_sha256,NULL,encoding_version,
       compression_level,compression_applied,created_at,created_at
FROM images_v1;

DROP TABLE images_v1;
DROP TABLE blobs_v1;

UPDATE sqlite_sequence
SET seq=(SELECT COALESCE(MAX(seq),0) FROM images)
WHERE name='images';
