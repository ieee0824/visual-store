use super::{
    Store, check_store, connect, manifest, migration_incomplete, migration_journal, migration_v3,
};
use crate::{
    Error, Result,
    error::{integrity, invalid},
    fault,
    filesystem::Dir,
};
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};
use std::{collections::HashSet, fs, path::Path};

const RESULT_EXAMPLES: usize = 20;

#[derive(Debug, Clone)]
pub struct PruneOptions {
    pub dry_run: bool,
    pub apply: bool,
    pub max_objects: usize,
}

pub struct PruneOutcome {
    pub data: Value,
    pub error: Option<Error>,
}

#[derive(Debug)]
struct Candidate {
    hash: String,
    bytes: u64,
    physical_bytes: u64,
}

struct Report {
    dry_run: bool,
    active_representation_bytes: u64,
    source_retained_bytes: u64,
    retired_candidate_bytes: u64,
    reclaimable_bytes: u64,
    reclaimed_bytes: u64,
    physical_object_bytes_before: u64,
    physical_object_bytes_after: u64,
    candidate_objects: u64,
    pruned_objects: u64,
    result_count: u64,
    results: Vec<Value>,
}

impl Report {
    fn push(&mut self, value: Value) {
        self.result_count += 1;
        if self.results.len() < RESULT_EXAMPLES {
            self.results.push(value);
        }
    }
    fn value(self) -> Value {
        json!({
            "dry_run":self.dry_run,
            "active_representation_bytes":self.active_representation_bytes,
            "source_retained_bytes":self.source_retained_bytes,
            "retired_candidate_bytes":self.retired_candidate_bytes,
            "reclaimable_bytes":self.reclaimable_bytes,
            "reclaimed_bytes":self.reclaimed_bytes,
            "physical_object_bytes_before":self.physical_object_bytes_before,
            "physical_object_bytes_after":self.physical_object_bytes_after,
            "candidate_objects":self.candidate_objects,
            "pruned_objects":self.pruned_objects,
            "result_count":self.result_count,
            "results":self.results,
        })
    }
}

fn checked_sum(values: impl IntoIterator<Item = u64>) -> Result<u64> {
    values
        .into_iter()
        .try_fold(0u64, u64::checked_add)
        .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Capacity total overflow."))
}

fn physical_object_bytes(root: &Dir) -> Result<u64> {
    let objects = root.child("objects", false)?.child("sha256", false)?;
    let mut total = 0u64;
    for first in fs::read_dir(&objects.path)? {
        let first = first?;
        if !first.file_type()?.is_dir() {
            return Err(integrity("Unexpected object entry."));
        }
        for second in fs::read_dir(first.path())? {
            let second = second?;
            if !second.file_type()?.is_dir() {
                return Err(integrity("Unexpected object entry."));
            }
            for object in fs::read_dir(second.path())? {
                let object = object?;
                if !object.file_type()?.is_file() {
                    return Err(integrity("Unexpected object entry."));
                }
                total = total
                    .checked_add(object.metadata()?.len())
                    .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Physical capacity overflow."))?;
            }
        }
    }
    Ok(total)
}

impl Store {
    fn capacity(&self, sql: &str) -> Result<u64> {
        Ok(self.conn.query_row(sql, [], |row| row.get(0))?)
    }

    fn verify_replacements(&self, hash: &str, verified: &mut HashSet<String>) -> Result<()> {
        let total: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM retired_representations WHERE representation_kind='png' AND png_blob_sha256=?1 AND prune_state!='deleted'",
            [hash], |row| row.get(0),
        )?;
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT r.segment_id FROM retired_representations rr JOIN representations r ON r.image_id=rr.image_id AND r.representation_kind='vp9_segment' WHERE rr.representation_kind='png' AND rr.png_blob_sha256=?1 AND rr.prune_state!='deleted'"
        )?;
        let segments = statement
            .query_map([hash], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let covered: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM retired_representations rr JOIN representations r ON r.image_id=rr.image_id AND r.representation_kind='vp9_segment' WHERE rr.representation_kind='png' AND rr.png_blob_sha256=?1 AND rr.prune_state!='deleted'",
            [hash], |row| row.get(0),
        )?;
        if total == 0 || covered != total || segments.is_empty() {
            return Err(integrity("Retired PNG replacement mapping is incomplete."));
        }
        for segment in segments {
            if verified.insert(segment.clone()) {
                self.verify_temporal_segment(&segment)?;
            }
        }
        Ok(())
    }

    fn prune_locked(&mut self, options: PruneOptions) -> Result<PruneOutcome> {
        if options.dry_run == options.apply {
            return Err(invalid("Choose exactly one of --dry-run or --apply."));
        }
        if options.max_objects == 0 {
            return Err(invalid("Prune limits must be positive."));
        }
        let active = self.capacity(
            "SELECT COALESCE(SUM(byte_length),0) FROM blobs WHERE sha256 IN (SELECT png_blob_sha256 FROM representations WHERE representation_kind='png' UNION SELECT s.color_blob_sha256 FROM segments s JOIN representations r ON r.segment_id=s.segment_id UNION SELECT s.alpha_blob_sha256 FROM segments s JOIN representations r ON r.segment_id=s.segment_id WHERE s.alpha_blob_sha256 IS NOT NULL UNION SELECT pr.descriptor_blob_sha256 FROM png_reconstruction pr JOIN representations r ON r.image_id=pr.image_id AND r.representation_kind='vp9_segment')"
        )?;
        let sources = self.capacity(
            "SELECT COALESCE(SUM(byte_length),0) FROM blobs WHERE sha256 IN (SELECT source_blob_sha256 FROM images WHERE source_blob_sha256 IS NOT NULL)"
        )?;
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT rr.png_blob_sha256,b.byte_length FROM retired_representations rr JOIN blobs b ON b.sha256=rr.png_blob_sha256 AND b.object_kind='png' WHERE rr.representation_kind='png' AND rr.prune_state!='deleted' AND NOT EXISTS(SELECT 1 FROM representations r WHERE r.representation_kind='png' AND r.png_blob_sha256=rr.png_blob_sha256) AND NOT EXISTS(SELECT 1 FROM images i WHERE i.source_blob_sha256=rr.png_blob_sha256) AND NOT EXISTS(SELECT 1 FROM segments s WHERE s.color_blob_sha256=rr.png_blob_sha256 OR s.alpha_blob_sha256=rr.png_blob_sha256) AND NOT EXISTS(SELECT 1 FROM png_reconstruction pr WHERE pr.descriptor_blob_sha256=rr.png_blob_sha256) ORDER BY rr.png_blob_sha256"
        )?;
        let mut candidates = statement
            .query_map([], |row| {
                Ok(Candidate {
                    hash: row.get(0)?,
                    bytes: row.get(1)?,
                    physical_bytes: 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        for candidate in &mut candidates {
            let path = self
                .blob_dir(&candidate.hash, false)?
                .path
                .join(format!("{}.png", candidate.hash));
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() => candidate.physical_bytes = metadata.len(),
                Ok(_) => return Err(integrity("Prune candidate path is not a regular file.")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if candidates.len() > options.max_objects {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Prune candidate count exceeds --max-prune-objects.",
            ));
        }
        let retired_bytes = checked_sum(candidates.iter().map(|candidate| candidate.bytes))?;
        let reclaimable_bytes =
            checked_sum(candidates.iter().map(|candidate| candidate.physical_bytes))?;
        let physical_before = physical_object_bytes(&self.root)?;
        let mut report = Report {
            dry_run: options.dry_run,
            active_representation_bytes: active,
            source_retained_bytes: sources,
            retired_candidate_bytes: retired_bytes,
            reclaimable_bytes,
            reclaimed_bytes: 0,
            physical_object_bytes_before: physical_before,
            physical_object_bytes_after: physical_before,
            candidate_objects: candidates.len() as u64,
            pruned_objects: 0,
            result_count: 0,
            results: Vec::new(),
        };
        let mut verified = HashSet::new();
        for candidate in candidates {
            if let Err(error) = self.verify_replacements(&candidate.hash, &mut verified) {
                report.push(json!({"blob_sha256":candidate.hash,"byte_length":candidate.bytes,"status":"failed","reason":error.code}));
                return Ok(PruneOutcome {
                    data: report.value(),
                    error: Some(error),
                });
            }
            if options.dry_run {
                report.push(json!({"blob_sha256":candidate.hash,"byte_length":candidate.bytes,"status":"reclaimable","reason":"verified_replacement"}));
                continue;
            }
            fault("prune_before_tombstone")?;
            let transaction = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let still_safe: bool = transaction.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM representations WHERE representation_kind='png' AND png_blob_sha256=?1) AND NOT EXISTS(SELECT 1 FROM images WHERE source_blob_sha256=?1)",
                [&candidate.hash], |row| row.get(0),
            )?;
            if !still_safe {
                transaction.rollback()?;
                return Err(integrity("Prune candidate became referenced."));
            }
            transaction.execute(
                "UPDATE retired_representations SET prune_state='pending' WHERE representation_kind='png' AND png_blob_sha256=?1 AND prune_state='retained'",
                [&candidate.hash],
            )?;
            transaction.commit()?;
            fault("prune_after_tombstone")?;
            fault("prune_before_delete")?;
            let dir = self.blob_dir(&candidate.hash, false)?;
            let removed = dir.remove_file_if_exists(&format!("{}.png", candidate.hash))?;
            fault("prune_after_delete")?;
            fault("prune_before_finalize")?;
            let transaction = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "UPDATE retired_representations SET prune_state='deleted' WHERE representation_kind='png' AND png_blob_sha256=?1 AND prune_state='pending'",
                [&candidate.hash],
            )?;
            transaction.commit()?;
            fault("prune_after_finalize")?;
            if removed {
                report.reclaimed_bytes = report
                    .reclaimed_bytes
                    .checked_add(candidate.physical_bytes)
                    .ok_or_else(|| {
                        Error::new("E_LIMIT_EXCEEDED", "Reclaimed byte total overflow.")
                    })?;
            }
            report.pruned_objects += 1;
            report.push(json!({"blob_sha256":candidate.hash,"byte_length":candidate.bytes,"status":"pruned","reason":if removed {"file_removed"} else {"pending_file_already_absent"}}));
        }
        report.physical_object_bytes_after = physical_object_bytes(&self.root)?;
        Ok(PruneOutcome {
            data: report.value(),
            error: None,
        })
    }

    pub fn prune(
        path: &Path,
        options: PruneOptions,
        limits: crate::image::Limits,
    ) -> Result<PruneOutcome> {
        if !path.try_exists()? {
            return Err(Error::new(
                "E_STORE_NOT_INITIALIZED",
                "Initialize the selected store first.",
            ));
        }
        let root = Dir::open(path)?;
        root.lock(true)?;
        if migration_journal(&root)?.is_some() || migration_v3::journal_exists(&root)? {
            return Err(migration_incomplete());
        }
        let manifest = manifest(&root)?;
        if manifest.format_version < 2 {
            return Err(Error::new(
                "E_SCHEMA_VERSION",
                "Prune requires a version 2 store.",
            ));
        }
        let conn = connect(&root, true)?;
        check_store(&root, &conn, &manifest)?;
        let mut store = Store {
            conn,
            root,
            id: manifest.store_id,
            format_version: manifest.format_version,
            limits,
        };
        store.prune_locked(options)
    }
}
