DROP INDEX images_by_run_seq;
DROP INDEX images_by_stored_blob;
DROP INDEX images_by_source_blob;
ALTER TABLE images RENAME TO images_v1;
ALTER TABLE blobs RENAME TO blobs_v1;
