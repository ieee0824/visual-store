use crate::{
    Error, Result,
    error::{integrity, invalid},
    fault,
    filesystem::{self, Dir, Temp},
    image::{self, Limits, reconstruction::ReconstructionMetadata},
    segment::{self, SegmentLimits},
    sha256,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, backup::Backup, params,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path, time::Duration};
use uuid::Uuid;

mod pack;
pub use pack::{PackOptions, PackOutcome};

const ENCODING: &str = "png-idat-zlib-v1";
const CURRENT_FORMAT_VERSION: u32 = 2;
const MIGRATION_JOURNAL: &str = "migration-v1-to-v2.json";
const MIGRATION_BACKUP: &str = "migration-v1-backup.sqlite3";
const MIGRATION_BACKUP_FILES: [&str; 4] = [
    "migration-v1-backup.sqlite3-wal",
    "migration-v1-backup.sqlite3-shm",
    "migration-v1-backup.sqlite3-journal",
    MIGRATION_BACKUP,
];
// Reserve space for the CLI success envelope and trailing newline so list stdout
// stays within the documented 16 KiB budget.
const LIST_DATA_BUDGET_BYTES: usize = 16 * 1024 - 64;
const LIST_ALL_SQL_V1: &str = "SELECT image_id,seq,run,NULL,NULL,label,width,height,created_at,(SELECT byte_length FROM blobs WHERE sha256=stored_blob_sha256) FROM images WHERE seq<=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3";
const LIST_RUN_SQL_V1: &str = "SELECT image_id,seq,run,NULL,NULL,label,width,height,created_at,(SELECT byte_length FROM blobs WHERE sha256=stored_blob_sha256) FROM images WHERE run=?1 AND seq<=?2 AND seq<?3 ORDER BY seq DESC LIMIT ?4";
const LIST_ALL_SQL_V2: &str = "SELECT i.image_id,i.seq,i.run,i.stream,i.frame_no,i.label,i.width,i.height,i.created_at,b.byte_length FROM images i JOIN representations r ON r.image_id=i.image_id LEFT JOIN retired_representations rr ON rr.retired_id=(SELECT retired_id FROM retired_representations WHERE image_id=i.image_id AND representation_kind='png' AND prune_state!='deleted' ORDER BY representation_version DESC LIMIT 1) JOIN blobs b ON b.sha256=CASE WHEN r.representation_kind='png' THEN r.png_blob_sha256 ELSE rr.png_blob_sha256 END WHERE i.seq<=?1 AND i.seq<?2 ORDER BY i.seq DESC LIMIT ?3";
const LIST_RUN_SQL_V2: &str = "SELECT i.image_id,i.seq,i.run,i.stream,i.frame_no,i.label,i.width,i.height,i.created_at,b.byte_length FROM images i JOIN representations r ON r.image_id=i.image_id LEFT JOIN retired_representations rr ON rr.retired_id=(SELECT retired_id FROM retired_representations WHERE image_id=i.image_id AND representation_kind='png' AND prune_state!='deleted' ORDER BY representation_version DESC LIMIT 1) JOIN blobs b ON b.sha256=CASE WHEN r.representation_kind='png' THEN r.png_blob_sha256 ELSE rr.png_blob_sha256 END WHERE i.run=?1 AND i.seq<=?2 AND i.seq<?3 ORDER BY i.seq DESC LIMIT ?4";
type PriorOperation = (String, String, u32, Option<String>, Option<String>);
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    store_id: Uuid,
    format_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationState {
    Started,
    DbCommitted,
    ManifestUpdated,
    Restoring,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationJournal {
    journal_version: u32,
    store_id: Uuid,
    from_version: u32,
    to_version: u32,
    state: MigrationState,
    backup_file: String,
    started_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PutOptions {
    pub run: Option<String>,
    pub stream: Option<String>,
    pub label: Option<String>,
    pub note: Option<String>,
    pub tags: Vec<String>,
    pub captured_at: Option<String>,
    pub keep_source: bool,
    pub operation_id: Option<String>,
    pub compression_level: u32,
    pub limits: Limits,
}
impl Default for PutOptions {
    fn default() -> Self {
        Self {
            run: None,
            stream: None,
            label: None,
            note: None,
            tags: vec![],
            captured_at: None,
            keep_source: false,
            operation_id: None,
            compression_level: 6,
            limits: Limits::default(),
        }
    }
}
fn bounded(value: &mut Option<String>, max: usize) -> Result<()> {
    if value.as_ref().is_some_and(|v| v.len() > max) {
        return Err(Error::new(
            "E_LIMIT_EXCEEDED",
            "Metadata exceeds byte limit.",
        ));
    }
    if value.as_deref() == Some("") {
        *value = None;
    }
    Ok(())
}
impl PutOptions {
    fn normalize(&mut self) -> Result<()> {
        bounded(&mut self.run, 128)?;
        if self.stream.as_ref().is_some_and(|stream| stream.is_empty()) {
            return Err(invalid("Stream requires 1–128 UTF-8 bytes."));
        }
        if self
            .stream
            .as_ref()
            .is_some_and(|stream| stream.len() > 128)
        {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Stream exceeds the 128-byte limit.",
            ));
        }
        match (&self.run, &self.stream) {
            (Some(_), None) => self.stream = Some("default".into()),
            (None, Some(_)) => return Err(invalid("Stream requires --run.")),
            _ => {}
        }
        bounded(&mut self.label, 256)?;
        bounded(&mut self.note, 2048)?;
        if self.tags.len() > 16 || self.tags.iter().any(|t| t.is_empty() || t.len() > 64) {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Tags require 1–64 bytes each, at most 16 tags.",
            ));
        }
        self.tags.sort();
        self.tags.dedup();
        if self
            .operation_id
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 128)
        {
            return Err(invalid("Operation ID requires 1–128 UTF-8 bytes."));
        }
        if self.compression_level > 9 {
            return Err(invalid("Compression level must be 0 through 9."));
        }
        if let Some(s) = &self.captured_at {
            self.captured_at = Some(
                DateTime::parse_from_rfc3339(s)
                    .map_err(|_| invalid("Invalid captured-at timestamp."))?
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Nanos, true),
            );
        }
        if serde_json::to_vec(&json!([
            self.run,
            self.stream,
            self.label,
            self.note,
            self.tags,
            self.captured_at
        ]))?
        .len()
            > 6000
        {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Escaped metadata exceeds the info JSON budget.",
            ));
        }
        self.limits.validate()
    }
    fn fingerprint_v1(&self, source: &str) -> Result<String> {
        // A versioned JSON array gives unambiguous field boundaries and stable ordering.
        Ok(sha256(&serde_json::to_vec(&json!([
            1,
            source,
            self.run,
            self.label,
            self.note,
            self.tags,
            self.captured_at,
            self.keep_source,
            self.compression_level
        ]))?))
    }

    fn fingerprint_v2(&self, source: &str) -> Result<String> {
        Ok(sha256(&serde_json::to_vec(&json!([
            2,
            source,
            self.run,
            self.stream,
            self.label,
            self.note,
            self.tags,
            self.captured_at,
            self.keep_source,
            self.compression_level
        ]))?))
    }
}

#[derive(Debug, Serialize)]
pub struct ImageRecord {
    pub image_id: String,
    pub seq: i64,
    pub run: Option<String>,
    pub stream: Option<String>,
    pub frame_no: Option<u64>,
    pub created_at: String,
    pub captured_at: Option<String>,
    pub label: Option<String>,
    pub note: Option<String>,
    pub tags: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub stored_sha256: String,
    pub stored_bytes: u64,
    pub source_retained: bool,
    pub scanline_sha256: String,
    pub non_idat_sha256: String,
    pub pixel_sha256: String,
    pub encoding_version: String,
    pub compression_level: u32,
    pub compression_applied: bool,
    #[serde(skip)]
    source_blob: Option<String>,
}
const SELECT_IMAGE_V1: &str = "SELECT i.image_id,i.seq,i.run,NULL,NULL,i.created_at,i.captured_at,i.label,i.note,i.tags_json,i.width,i.height,i.bit_depth,i.color_type,i.source_sha256,i.source_byte_length,i.stored_blob_sha256,b.byte_length,i.source_blob_sha256,i.scanline_sha256,i.non_idat_sha256,i.pixel_sha256,i.encoding_version,i.compression_level,i.compression_applied,'png' FROM images i JOIN blobs b ON b.sha256=i.stored_blob_sha256";
const SELECT_IMAGE_V2: &str = "SELECT i.image_id,i.seq,i.run,i.stream,i.frame_no,i.created_at,i.captured_at,i.label,i.note,i.tags_json,i.width,i.height,i.bit_depth,i.color_type,i.source_sha256,i.source_byte_length,CASE WHEN r.representation_kind='png' THEN r.png_blob_sha256 ELSE rr.png_blob_sha256 END,b.byte_length,i.source_blob_sha256,i.scanline_sha256,i.non_idat_sha256,i.pixel_sha256,CASE WHEN r.representation_kind='png' THEN r.encoding_version ELSE rr.encoding_version END,CASE WHEN r.representation_kind='png' THEN r.compression_level ELSE NULL END,CASE WHEN r.representation_kind='png' THEN r.compression_applied ELSE NULL END,r.representation_kind FROM images i JOIN representations r ON r.image_id=i.image_id LEFT JOIN retired_representations rr ON rr.retired_id=(SELECT retired_id FROM retired_representations WHERE image_id=i.image_id AND representation_kind='png' AND prune_state!='deleted' ORDER BY representation_version DESC LIMIT 1) JOIN blobs b ON b.sha256=CASE WHEN r.representation_kind='png' THEN r.png_blob_sha256 ELSE rr.png_blob_sha256 END";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredPngEncoding {
    version: u8,
    encoding_version: String,
    compression_level: u32,
    compression_applied: bool,
}

fn row_image(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImageRecord> {
    let tags: String = r.get(9)?;
    let source_blob: Option<String> = r.get(18)?;
    let active_kind: String = r.get(25)?;
    let encoding: String = r.get(22)?;
    let (encoding_version, compression_level, compression_applied) = if active_kind == "png" {
        (encoding, r.get(23)?, r.get(24)?)
    } else {
        let retired: RetiredPngEncoding = serde_json::from_str(&encoding).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                22,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        if retired.version != 1 {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                22,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unsupported retired PNG metadata",
                )),
            ));
        }
        (
            retired.encoding_version,
            retired.compression_level,
            retired.compression_applied,
        )
    };
    Ok(ImageRecord {
        image_id: r.get(0)?,
        seq: r.get(1)?,
        run: r.get(2)?,
        stream: r.get(3)?,
        frame_no: r.get(4)?,
        created_at: r.get(5)?,
        captured_at: r.get(6)?,
        label: r.get(7)?,
        note: r.get(8)?,
        tags: serde_json::from_str(&tags).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
        })?,
        width: r.get(10)?,
        height: r.get(11)?,
        bit_depth: r.get(12)?,
        color_type: r.get(13)?,
        source_sha256: r.get(14)?,
        source_bytes: r.get(15)?,
        stored_sha256: r.get(16)?,
        stored_bytes: r.get(17)?,
        source_retained: source_blob.is_some(),
        source_blob,
        scanline_sha256: r.get(19)?,
        non_idat_sha256: r.get(20)?,
        pixel_sha256: r.get(21)?,
        encoding_version,
        compression_level,
        compression_applied,
    })
}

pub struct Store {
    conn: Connection,
    root: Dir,
    id: Uuid,
    format_version: u32,
    pub limits: Limits,
}
fn connect(root: &Dir, writable: bool) -> Result<Connection> {
    root.check_sqlite_paths()?;
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let c = Connection::open_with_flags(
        root.path.join("index.sqlite3"),
        flags | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(Duration::from_secs(5))?;
    c.pragma_update(None, "foreign_keys", true)?;
    c.pragma_update(None, "trusted_schema", false)?;
    if writable {
        c.pragma_update(None, "synchronous", "FULL")?;
    }
    Ok(c)
}
fn manifest(root: &Dir) -> Result<Manifest> {
    let mut f = root.open_file("store.json").map_err(|_| {
        integrity(
            "Store manifest is missing or unsafe; partial initialization requires inspection.",
        )
    })?;
    let m: Manifest = serde_json::from_slice(&filesystem::read_bounded(&mut f, 4096)?)?;
    if !matches!(m.format_version, 1 | CURRENT_FORMAT_VERSION) {
        return Err(Error::new("E_SCHEMA_VERSION", "Unsupported store format."));
    }
    Ok(m)
}
fn database_version(c: &Connection) -> Result<u32> {
    Ok(c.pragma_query_value(None, "user_version", |row| row.get(0))?)
}
fn migration_journal(root: &Dir) -> Result<Option<MigrationJournal>> {
    let mut file = match root.open_file(MIGRATION_JOURNAL) {
        Ok(file) => file,
        Err(error)
            if error.code == "E_IO"
                && !root
                    .path
                    .join(MIGRATION_JOURNAL)
                    .try_exists()
                    .unwrap_or(true) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let journal: MigrationJournal =
        serde_json::from_slice(&filesystem::read_bounded(&mut file, 4096)?)?;
    if journal.journal_version != 1
        || journal.from_version != 1
        || journal.to_version != CURRENT_FORMAT_VERSION
        || journal.backup_file != MIGRATION_BACKUP
    {
        return Err(integrity("Invalid migration journal."));
    }
    Ok(Some(journal))
}
fn migration_incomplete() -> Error {
    Error::new(
        "E_MIGRATION_INCOMPLETE",
        "Store migration is incomplete; run migrate --to 2 --resume or --restore.",
    )
}
fn check_store(root: &Dir, c: &Connection, m: &Manifest) -> Result<()> {
    let version = database_version(c)?;
    if version != m.format_version || !matches!(version, 1 | CURRENT_FORMAT_VERSION) {
        return Err(Error::new(
            "E_SCHEMA_VERSION",
            "Manifest and database schema versions are unsupported or inconsistent.",
        ));
    }
    let id: String = c.query_row(
        "SELECT value FROM store_meta WHERE key='store_id'",
        [],
        |r| r.get(0),
    )?;
    if id != m.store_id.to_string() {
        return Err(integrity("Manifest/database store IDs differ."));
    }
    let wal: String = c.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if !wal.eq_ignore_ascii_case("wal") {
        return Err(integrity("Store database must use WAL."));
    }
    let managed = || -> Result<()> {
        root.child("objects", false)?.child("sha256", false)?;
        root.child("tmp", false)?;
        root.child("exports", false)?;
        Ok(())
    };
    managed().map_err(|_| integrity("A managed store directory is missing or unsafe."))?;
    Ok(())
}

fn write_json_atomic<T: Serialize>(root: &Dir, name: &str, value: &T) -> Result<()> {
    let mut temporary = Temp::new(root)?;
    temporary.file.write_all(&serde_json::to_vec(value)?)?;
    temporary.replace(root, name)
}

fn write_manifest(root: &Dir, manifest: &Manifest) -> Result<()> {
    write_json_atomic(root, "store.json", manifest)
}

fn write_migration_journal(root: &Dir, journal: &MigrationJournal) -> Result<()> {
    write_json_atomic(root, MIGRATION_JOURNAL, journal)
}

fn validate_v1_backup(path: &Path, expected_store_id: Uuid) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
        return Err(integrity("Unsafe migration backup path."));
    }
    let backup = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    if database_version(&backup)? != 1 {
        return Err(integrity(
            "Migration backup has an unexpected schema version.",
        ));
    }
    let store_id: String = backup.query_row(
        "SELECT value FROM store_meta WHERE key='store_id'",
        [],
        |row| row.get(0),
    )?;
    if store_id != expected_store_id.to_string() {
        return Err(integrity("Migration backup belongs to another store."));
    }
    let integrity_result: String =
        backup.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
    if integrity_result != "ok" {
        return Err(integrity("Migration backup failed SQLite integrity check."));
    }
    Ok(())
}

fn remove_migration_backup(root: &Dir) -> Result<()> {
    for name in MIGRATION_BACKUP_FILES {
        root.remove_file_if_exists(name)?;
    }
    Ok(())
}

fn create_v1_backup(root: &Dir, source: &Connection, store_id: Uuid) -> Result<()> {
    remove_migration_backup(root)?;
    drop(root.new_file(MIGRATION_BACKUP)?);
    let mut destination = Connection::open_with_flags(
        root.path.join(MIGRATION_BACKUP),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    Backup::new(source, &mut destination)?.run_to_completion(
        100,
        Duration::from_millis(10),
        None,
    )?;
    drop(destination);
    root.open_file(MIGRATION_BACKUP)?.sync_all()?;
    root.sync()?;
    validate_v1_backup(&root.path.join(MIGRATION_BACKUP), store_id)
}

fn apply_v2_schema(connection: &mut Connection, journal: &MigrationJournal) -> Result<()> {
    connection.pragma_update(None, "foreign_keys", false)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(include_str!("../migrations/001-to-002-prepare.sql"))?;
    transaction.execute_batch(include_str!("../migrations/002.sql"))?;
    transaction.execute_batch(include_str!("../migrations/001-to-002-copy.sql"))?;
    transaction.execute(
        "INSERT INTO schema_migrations(from_version,to_version,started_at,completed_at,backup_file) VALUES (1,2,?1,NULL,?2)",
        params![journal.started_at, journal.backup_file],
    )?;
    let foreign_key_errors: i64 =
        transaction.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        return Err(integrity(
            "Migrated database failed foreign-key validation.",
        ));
    }
    transaction.commit()?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

fn restore_v1(
    root: &Dir,
    connection: &mut Connection,
    manifest_value: &mut Manifest,
    journal: &mut MigrationJournal,
) -> Result<Value> {
    let restored_from_version = database_version(connection)?;
    validate_v1_backup(&root.path.join(MIGRATION_BACKUP), journal.store_id)?;
    journal.state = MigrationState::Restoring;
    write_migration_journal(root, journal)?;
    fault("migration_restore_started")?;
    let backup_connection = Connection::open_with_flags(
        root.path.join(MIGRATION_BACKUP),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    Backup::new(&backup_connection, connection)?.run_to_completion(
        100,
        Duration::from_millis(10),
        None,
    )?;
    drop(backup_connection);
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    fault("migration_restore_after_db")?;
    manifest_value.format_version = 1;
    write_manifest(root, manifest_value)?;
    fault("migration_restore_after_manifest")?;
    root.remove_file_if_exists(MIGRATION_JOURNAL)?;
    fault("migration_restore_after_journal_cleanup")?;
    remove_migration_backup(root)?;
    fault("migration_restore_after_cleanup")?;
    Ok(json!({
        "store_id": journal.store_id,
        "from_version": restored_from_version,
        "to_version": 1,
        "migrated": false,
        "resumed": true,
        "restored": true
    }))
}

impl Store {
    pub fn migrate(path: &Path, to: u32, resume: bool, restore: bool) -> Result<Value> {
        if to != CURRENT_FORMAT_VERSION {
            return Err(invalid("Only migration target 2 is supported."));
        }
        if resume && restore {
            return Err(invalid("Choose either --resume or --restore."));
        }
        if !path.try_exists()? {
            return Err(Error::new(
                "E_STORE_NOT_INITIALIZED",
                "Initialize the selected store first.",
            ));
        }
        let root = Dir::open(path)?;
        root.lock(true)?;
        root.check_sqlite_paths()?;
        let mut manifest_value = manifest(&root)?;
        let mut connection = connect(&root, true)?;
        let mut journal = migration_journal(&root)?;
        let initial_version = database_version(&connection)?;

        if journal.is_some() && !resume && !restore {
            return Err(migration_incomplete());
        }
        if restore && journal.is_none() {
            if manifest_value.format_version == 1
                && initial_version == 1
                && root.path.join(MIGRATION_BACKUP).try_exists()?
            {
                validate_v1_backup(&root.path.join(MIGRATION_BACKUP), manifest_value.store_id)?;
                remove_migration_backup(&root)?;
                return Ok(json!({
                    "store_id": manifest_value.store_id,
                    "from_version": 1,
                    "to_version": 1,
                    "migrated": false,
                    "resumed": true,
                    "restored": true
                }));
            }
            return Err(invalid("There is no incomplete migration to restore."));
        }

        if journal.is_none()
            && manifest_value.format_version == CURRENT_FORMAT_VERSION
            && initial_version == CURRENT_FORMAT_VERSION
        {
            return Ok(json!({
                "store_id": manifest_value.store_id,
                "from_version": CURRENT_FORMAT_VERSION,
                "to_version": CURRENT_FORMAT_VERSION,
                "migrated": false,
                "resumed": false,
                "restored": false
            }));
        }
        if journal.is_none() && (manifest_value.format_version != 1 || initial_version != 1) {
            return Err(migration_incomplete());
        }

        if journal.is_none() {
            create_v1_backup(&root, &connection, manifest_value.store_id)?;
            fault("migration_after_backup")?;
            let started = MigrationJournal {
                journal_version: 1,
                store_id: manifest_value.store_id,
                from_version: 1,
                to_version: CURRENT_FORMAT_VERSION,
                state: MigrationState::Started,
                backup_file: MIGRATION_BACKUP.into(),
                started_at: now(),
            };
            write_migration_journal(&root, &started)?;
            fault("migration_after_journal")?;
            journal = Some(started);
        }

        let mut journal = journal.expect("migration journal was initialized");
        if journal.store_id != manifest_value.store_id {
            return Err(integrity(
                "Migration journal store ID differs from manifest.",
            ));
        }
        if restore {
            return restore_v1(&root, &mut connection, &mut manifest_value, &mut journal);
        }

        let database_version = database_version(&connection)?;
        match database_version {
            1 => {
                if manifest_value.format_version != 1 {
                    return Err(migration_incomplete());
                }
                validate_v1_backup(&root.path.join(MIGRATION_BACKUP), journal.store_id)?;
                fault("migration_before_db_commit")?;
                apply_v2_schema(&mut connection, &journal)?;
                fault("migration_after_db_commit")?;
            }
            CURRENT_FORMAT_VERSION => {}
            _ => return Err(migration_incomplete()),
        }
        journal.state = MigrationState::DbCommitted;
        write_migration_journal(&root, &journal)?;
        fault("migration_after_db_journal")?;

        match manifest_value.format_version {
            1 => {
                manifest_value.format_version = CURRENT_FORMAT_VERSION;
                write_manifest(&root, &manifest_value)?;
                fault("migration_after_manifest")?;
            }
            CURRENT_FORMAT_VERSION => {}
            _ => return Err(migration_incomplete()),
        }
        journal.state = MigrationState::ManifestUpdated;
        write_migration_journal(&root, &journal)?;
        fault("migration_after_manifest_journal")?;

        let history_completion: Option<Option<String>> = connection
            .query_row(
                "SELECT completed_at FROM schema_migrations ORDER BY migration_id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match history_completion {
            Some(None) => {
                connection.execute(
                    "UPDATE schema_migrations SET completed_at=?1 WHERE migration_id=(SELECT MAX(migration_id) FROM schema_migrations)",
                    [now()],
                )?;
            }
            Some(Some(_)) => {}
            None => return Err(integrity("Migration history is missing.")),
        }
        fault("migration_before_cleanup")?;
        remove_migration_backup(&root)?;
        fault("migration_after_backup_cleanup")?;
        root.remove_file_if_exists(MIGRATION_JOURNAL)?;
        fault("migration_after_cleanup")?;
        Ok(json!({
            "store_id": manifest_value.store_id,
            "from_version": 1,
            "to_version": CURRENT_FORMAT_VERSION,
            "migrated": true,
            "resumed": resume,
            "restored": false
        }))
    }

    pub fn initialize(path: &Path) -> Result<Value> {
        let root = Dir::create_root(path)?;
        root.lock(true)?;
        if fs::read_dir(&root.path)?.next().is_some() {
            if migration_journal(&root)?.is_some() {
                return Err(migration_incomplete());
            }
            let m = manifest(&root)?;
            let c = connect(&root, false)?;
            check_store(&root, &c, &m)?;
            return Ok(json!({
                "store_id":m.store_id,
                "schema_version":m.format_version,
                "already_initialized":true
            }));
        }
        let m = Manifest {
            store_id: Uuid::new_v4(),
            format_version: CURRENT_FORMAT_VERSION,
        };
        root.child("objects", true)?.child("sha256", true)?;
        root.child("tmp", true)?;
        root.child("exports", true)?;
        root.new_file("index.sqlite3")?.sync_all()?;
        let mut c = connect(&root, true)?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        tx.execute_batch(include_str!("../migrations/002.sql"))?;
        tx.execute(
            "INSERT INTO store_meta VALUES ('store_id',?1)",
            [m.store_id.to_string()],
        )?;
        tx.commit()?;
        fault("init_before_manifest")?;
        let mut temp = Temp::new(&root)?;
        temp.file.write_all(&serde_json::to_vec(&m)?)?;
        if !temp.publish(&root, "store.json")? {
            return Err(integrity("Manifest unexpectedly exists."));
        }
        root.sync()?;
        Ok(
            json!({"store_id":m.store_id,"schema_version":CURRENT_FORMAT_VERSION,"already_initialized":false}),
        )
    }
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        if !path.try_exists()? {
            return Err(Error::new(
                "E_STORE_NOT_INITIALIZED",
                "Initialize the selected store first.",
            ));
        }
        let root = Dir::open(path)?;
        root.lock(false)?;
        if fs::read_dir(&root.path)?.next().is_none() {
            return Err(Error::new(
                "E_STORE_NOT_INITIALIZED",
                "Initialize the selected store first.",
            ));
        }
        if migration_journal(&root)?.is_some() {
            return Err(migration_incomplete());
        }
        let m = manifest(&root)?;
        if writable && m.format_version == 1 {
            return Err(Error::new(
                "E_SCHEMA_VERSION",
                "Version 1 stores are read-only; run migrate --to 2.",
            ));
        }
        let conn = connect(&root, writable)?;
        check_store(&root, &conn, &m)?;
        Ok(Self {
            conn,
            root,
            id: m.store_id,
            format_version: m.format_version,
            limits: Limits::default(),
        })
    }
    pub fn store_id(&self) -> Uuid {
        self.id
    }
    fn reference(&self, id: &str) -> String {
        format!("visual://{}/images/{id}", self.id)
    }
    fn resolve(&self, reference: &str) -> Result<String> {
        let image_id = if let Some(tail) = reference.strip_prefix("visual://") {
            let parts: Vec<_> = tail.split('/').collect();
            if parts.len() != 3 || parts[1] != "images" {
                return Err(invalid("Invalid visual reference."));
            }
            let store = Uuid::parse_str(parts[0]).map_err(|_| invalid("Invalid store UUID."))?;
            if store != self.id {
                return Err(Error::new(
                    "E_STORE_MISMATCH",
                    "Reference belongs to another store.",
                ));
            }
            parts[2]
        } else {
            reference
        };
        Ok(Uuid::parse_str(image_id)
            .map_err(|_| invalid("Invalid image UUID."))?
            .to_string())
    }
    fn record(&self, id: &str) -> Result<ImageRecord> {
        let select = if self.format_version == 1 {
            SELECT_IMAGE_V1
        } else {
            SELECT_IMAGE_V2
        };
        self.conn
            .query_row(&format!("{select} WHERE i.image_id=?1"), [id], row_image)
            .optional()?
            .ok_or_else(|| Error::new("E_NOT_FOUND", "Image reference not found."))
    }
    pub fn info(&self, reference: &str) -> Result<Value> {
        let r = self.record(&self.resolve(reference)?)?;
        let mut v = serde_json::to_value(&r)?;
        v["ref"] = self.reference(&r.image_id).into();
        Ok(v)
    }
    fn blob_dir(&self, hash: &str, create: bool) -> Result<Dir> {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(integrity("Invalid blob hash."));
        }
        self.root
            .child("objects", false)?
            .child("sha256", false)?
            .child(&hash[..2], create)?
            .child(&hash[2..4], create)
    }
    fn blob_path(hash: &str) -> String {
        Self::object_path(hash, "png")
    }
    fn object_path(hash: &str, extension: &str) -> String {
        format!(
            "objects/sha256/{}/{}/{}.{}",
            &hash[..2],
            &hash[2..4],
            hash,
            extension
        )
    }
    fn blob_bytes(&self, hash: &str, length: u64) -> Result<Vec<u8>> {
        self.object_bytes(hash, length, "png", self.limits.source_bytes)
    }
    fn object_bytes(
        &self,
        hash: &str,
        length: u64,
        extension: &str,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let dir = self
            .blob_dir(hash, false)
            .map_err(|_| integrity("Blob directory missing or unsafe."))?;
        let mut f = dir
            .open_file(&format!("{hash}.{extension}"))
            .map_err(|_| integrity("Blob missing or unsafe."))?;
        filesystem::check_hash(&mut f, length, hash, limit)
    }
    fn save_object(&self, bytes: &[u8], extension: &str) -> Result<(String, bool)> {
        let hash = sha256(bytes);
        let dir = self.blob_dir(&hash, true)?;
        let tmp = self.root.child("tmp", false)?;
        let mut temp = Temp::new(&tmp)?;
        temp.file.write_all(bytes)?;
        temp.file.sync_all()?;
        let reused = !temp.publish(&dir, &format!("{hash}.{extension}"))?;
        if reused {
            let mut file = dir.open_file(&format!("{hash}.{extension}"))?;
            filesystem::check_hash(&mut file, bytes.len() as u64, &hash, bytes.len())?;
            dir.sync()?;
        }
        Ok((hash, reused))
    }
    fn save_blob(&self, bytes: &[u8]) -> Result<(String, bool)> {
        let hash = sha256(bytes);
        let dir = self.blob_dir(&hash, true)?;
        let tmp = self.root.child("tmp", false)?;
        let mut temp = Temp::new(&tmp)?;
        fault("blob_write")?;
        temp.file.write_all(bytes)?;
        temp.file.sync_all()?;
        fault("before_blob_publish")?;
        let reused = !temp.publish(&dir, &format!("{hash}.png"))?;
        if reused {
            let mut file = dir.open_file(&format!("{hash}.png"))?;
            filesystem::check_hash(&mut file, bytes.len() as u64, &hash, bytes.len())?;
            // The winning writer may not yet have synced the parent directory.
            dir.sync()?;
        }
        fault("after_blob_publish")?;
        Ok((hash, reused))
    }
    fn put_result(&self, id: &str, blob_reused: bool, record_reused: bool) -> Result<Value> {
        let r = self.record(id)?;
        Ok(
            json!({"ref":self.reference(&r.image_id),"image_id":r.image_id,"run":r.run,"stream":r.stream,"frame_no":r.frame_no,"seq":r.seq,
            "width":r.width,"height":r.height,"source_bytes":r.source_bytes,"stored_bytes":r.stored_bytes,
            "source_retained":r.source_retained,"blob_reused":blob_reused,"record_reused":record_reused,"compression_applied":r.compression_applied}),
        )
    }
    pub fn put(&mut self, source: &Path, mut opts: PutOptions) -> Result<Value> {
        opts.normalize()?;
        let tmp = self.root.child("tmp", false)?;
        let read_budget = opts.limits.memory_bytes.saturating_sub(16 * 1024 * 1024) / 3;
        let bytes = filesystem::snapshot(source, &tmp, opts.limits.source_bytes.min(read_budget))?;
        let source_hash = sha256(&bytes);
        let fingerprint_v1 = opts.fingerprint_v1(&source_hash)?;
        let fingerprint_v2 = opts.fingerprint_v2(&source_hash)?;
        let matches_prior =
            |fingerprint: &str, version: u32, run: &Option<String>, stream: &Option<String>| {
                run == &opts.run
                    && stream == &opts.stream
                    && match version {
                        1 => fingerprint == fingerprint_v1,
                        2 => fingerprint == fingerprint_v2,
                        _ => false,
                    }
            };
        if let Some(op) = &opts.operation_id {
            let prior: Option<PriorOperation> = self
                .conn
                .query_row(
                    "SELECT image_id,operation_fingerprint,operation_fingerprint_version,run,stream FROM images WHERE operation_id=?1",
                    [op],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((id, fingerprint, version, run, stream)) = prior {
                if !matches_prior(&fingerprint, version, &run, &stream) {
                    return Err(Error::new(
                        "E_CONFLICT",
                        "Operation ID has different registration content.",
                    ));
                }
                return self.put_result(&id, true, true);
            }
        }
        let packed = image::repack(&bytes, opts.compression_level, &opts.limits)?;
        let (stored_hash, reused) = self.save_blob(&packed.bytes)?;
        let source_blob = if opts.keep_source {
            if source_hash != stored_hash {
                self.save_blob(&bytes)?;
            }
            Some(source_hash.clone())
        } else {
            None
        };
        fault("before_db_commit")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(op) = &opts.operation_id {
            let prior: Option<PriorOperation> = tx
                .query_row(
                    "SELECT image_id,operation_fingerprint,operation_fingerprint_version,run,stream FROM images WHERE operation_id=?1",
                    [op],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((id, fingerprint, version, run, stream)) = prior {
                if !matches_prior(&fingerprint, version, &run, &stream) {
                    return Err(Error::new(
                        "E_CONFLICT",
                        "Operation ID has different registration content.",
                    ));
                }
                tx.rollback()?;
                return self.put_result(&id, true, true);
            }
        }
        let created = now();
        for (hash, n) in std::iter::once((&stored_hash, packed.bytes.len()))
            .chain(source_blob.as_ref().map(|h| (h, bytes.len())))
        {
            let relative = Self::blob_path(hash);
            tx.execute("INSERT INTO blobs(sha256,relative_path,byte_length,media_type,object_kind,created_at) VALUES (?1,?2,?3,'image/png','png',?4) ON CONFLICT(sha256) DO NOTHING",params![hash,relative,n as u64,created])?;
            let valid: bool = tx.query_row("SELECT relative_path=?2 AND byte_length=?3 AND media_type='image/png' AND object_kind='png' FROM blobs WHERE sha256=?1",params![hash,relative,n as u64],|r|r.get(0))?;
            if !valid {
                return Err(integrity("Existing blob metadata differs."));
            }
        }
        let id = Uuid::new_v4().to_string();
        let m = &packed.meta;
        let frame_no: Option<i64> = match (&opts.run, &opts.stream) {
            (Some(run), Some(stream)) => Some(tx.query_row(
                "SELECT COALESCE(MAX(frame_no),-1)+1 FROM images WHERE run=?1 AND stream=?2",
                params![run, stream],
                |row| row.get(0),
            )?),
            (None, None) => None,
            _ => return Err(integrity("Normalized run/stream state is inconsistent.")),
        };
        tx.execute("INSERT INTO images(image_id,run,stream,frame_no,created_at,captured_at,label,note,tags_json,width,height,bit_depth,color_type,source_sha256,source_byte_length,source_blob_sha256,scanline_sha256,non_idat_sha256,pixel_sha256,operation_id,operation_fingerprint,operation_fingerprint_version,validation_limits_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,2,?22)",params![id,opts.run,opts.stream,frame_no,created,opts.captured_at,opts.label,opts.note,serde_json::to_string(&opts.tags)?,m.width,m.height,m.bit_depth,m.color_type,source_hash,bytes.len() as u64,source_blob,m.scanline_sha256,m.non_idat_sha256,m.pixel_sha256,opts.operation_id,fingerprint_v2,serde_json::to_string(&opts.limits)?])?;
        tx.execute("INSERT INTO representations(image_id,representation_version,representation_kind,png_blob_sha256,segment_id,encoding_version,compression_level,compression_applied,created_at,verified_at) VALUES (?1,1,'png',?2,NULL,?3,?4,?5,?6,?6)",params![id,stored_hash,ENCODING,opts.compression_level,packed.compression_applied,created])?;
        fault("during_db_commit")?;
        tx.commit()?;
        fault("after_db_commit")?;
        self.put_result(&id, reused, false)
    }
    fn list_item(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
        Ok(json!({
            "ref": self.reference(&row.get::<_, String>(0)?),
            "seq": row.get::<_, i64>(1)?,
            "run": row.get::<_, Option<String>>(2)?,
            "stream": row.get::<_, Option<String>>(3)?,
            "frame_no": row.get::<_, Option<u64>>(4)?,
            "label": row.get::<_, Option<String>>(5)?,
            "width": row.get::<_, u32>(6)?,
            "height": row.get::<_, u32>(7)?,
            "created_at": row.get::<_, String>(8)?,
            "stored_bytes": row.get::<_, u64>(9)?,
        }))
    }
    pub fn list(&self, mut run: Option<String>, limit: u32, cursor: Option<&str>) -> Result<Value> {
        bounded(&mut run, 128)?;
        if !(1..=100).contains(&limit) {
            return Err(invalid("Limit must be 1 through 100."));
        }
        let c = if let Some(raw) = cursor {
            let err = || {
                Error::new(
                    "E_INVALID_CURSOR",
                    "Cursor is invalid or belongs to another query.",
                )
            };
            if raw.len() > 4096 {
                return Err(err());
            }
            let c: Cursor =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(raw).map_err(|_| err())?)
                    .map_err(|_| err())?;
            if c.version != 1 || c.store != self.id || c.run != run || c.last <= 0 || c.last > c.max
            {
                return Err(err());
            }
            c
        } else {
            let max = self
                .conn
                .query_row("SELECT COALESCE(MAX(seq),0) FROM images", [], |r| r.get(0))?;
            Cursor {
                version: 1,
                store: self.id,
                run: run.clone(),
                max,
                last: i64::MAX,
            }
        };
        let (all_sql, run_sql) = if self.format_version == 1 {
            (LIST_ALL_SQL_V1, LIST_RUN_SQL_V1)
        } else {
            (LIST_ALL_SQL_V2, LIST_RUN_SQL_V2)
        };
        let candidates = if let Some(run) = run.as_deref() {
            let mut stmt = self.conn.prepare(run_sql)?;
            let rows = stmt.query_map(params![run, c.max, c.last, limit + 1], |row| {
                self.list_item(row)
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self.conn.prepare(all_sql)?;
            let rows =
                stmt.query_map(params![c.max, c.last, limit + 1], |row| self.list_item(row))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let page = |items: &[Value], more: bool| -> Result<Value> {
            let mut result = json!({"items":items});
            if more {
                let last = result["items"]
                    .as_array()
                    .and_then(|items| items.last())
                    .and_then(|item| item["seq"].as_i64())
                    .ok_or_else(|| integrity("List pagination produced an empty page."))?;
                result["next_cursor"] = URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&Cursor {
                        version: c.version,
                        store: c.store,
                        run: c.run.clone(),
                        max: c.max,
                        last,
                    })?)
                    .into();
            }
            Ok(result)
        };
        let mut items = Vec::with_capacity((limit as usize).min(candidates.len()));
        for item in candidates.iter().take(limit as usize) {
            items.push(item.clone());
            let trial = page(&items, items.len() < candidates.len())?;
            if serde_json::to_vec(&trial)?.len() > LIST_DATA_BUDGET_BYTES {
                let _ = items.pop();
                if items.is_empty() {
                    return Err(Error::new(
                        "E_LIMIT_EXCEEDED",
                        "A list item exceeds the output byte budget.",
                    ));
                }
                break;
            }
        }
        page(&items, items.len() < candidates.len())
    }
    pub fn materialize(
        &self,
        reference: &str,
        source: bool,
        output: Option<&Path>,
    ) -> Result<Value> {
        let r = self.record(&self.resolve(reference)?)?;
        let (hash, length) = if source {
            (
                r.source_blob.as_ref().ok_or_else(|| {
                    Error::new("E_SOURCE_NOT_RETAINED", "Original bytes were not retained.")
                })?,
                r.source_bytes,
            )
        } else {
            (&r.stored_sha256, r.stored_bytes)
        };
        let (dir, name) = if let Some(out) = output {
            filesystem::external_destination(out)?
        } else {
            (
                self.root.child("exports", false)?,
                format!("{}-{}.png", r.image_id, Uuid::new_v4()),
            )
        };
        let out = dir.path.join(&name);
        let path = filesystem::path_json(&out)?.to_owned();
        // Prevent a long JSON-escaped destination from making the success response oversized.
        if serde_json::to_string(&path)?.len() > 3000 {
            return Err(invalid("Output path is too long for the JSON contract."));
        }
        filesystem::ensure_absent(&dir, &name)?;
        let bytes = self.blob_bytes(hash, length)?;
        let mut tmp = Temp::new(&dir)?;
        tmp.file.write_all(&bytes)?;
        if !tmp.publish(&dir, &name)? {
            return Err(Error::new("E_OUTPUT_EXISTS", "Output already exists."));
        }
        Ok(
            json!({"ref":self.reference(&r.image_id),"path":path,"media_type":"image/png","width":r.width,"height":r.height,"byte_length":length,"sha256":hash,"variant":if source {"source"} else {"stored"},"displayed":false}),
        )
    }

    pub fn verify(&self, report: Option<&Path>) -> Result<Value> {
        // Deferred transaction is pinned by the first read, before enumerating any files.
        let tx = self.conn.unchecked_transaction()?;
        tx.query_row("SELECT COUNT(*) FROM store_meta", [], |r| {
            r.get::<_, i64>(0)
        })?;
        check_store(&self.root, &tx, &manifest(&self.root)?)?;
        let destination = report.map(filesystem::external_destination).transpose()?;
        if let Some((dir, name)) = &destination {
            filesystem::ensure_absent(dir, name)?;
        }
        let mut report_file = destination
            .as_ref()
            .map(|(dir, _)| Temp::new(dir))
            .transpose()?;
        if let Some(file) = &mut report_file {
            file.file.write_all(b"{\"schema_version\":1,\"issues\":[")?;
        }
        let mut examples = Vec::new();
        let mut issue_count = 0u64;
        let mut errors = 0u64;
        let mut candidates = 0u64;
        let mut record_issue = |issue: Value, is_error: bool| -> Result<()> {
            if let Some(f) = &mut report_file {
                if issue_count > 0 {
                    f.file.write_all(b",")?;
                }
                f.file.write_all(&serde_json::to_vec(&issue)?)?;
            }
            issue_count += 1;
            if is_error {
                errors += 1;
            } else {
                candidates += 1;
            }
            if examples.len() < 20 {
                examples.push(issue);
            }
            Ok(())
        };
        {
            let mut stmt = tx.prepare("PRAGMA integrity_check")?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                if row? != "ok" {
                    record_issue(json!({"code":"database_integrity"}), true)?;
                }
            }
            let mut stmt = tx.prepare("PRAGMA foreign_key_check")?;
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {
                record_issue(json!({"code":"foreign_key_integrity"}), true)?;
            }
        }
        let mut blob_count = 0u64;
        let blob_query = if self.format_version == 1 {
            "SELECT sha256,relative_path,byte_length,media_type,'png' FROM blobs ORDER BY sha256"
        } else {
            "SELECT sha256,relative_path,byte_length,media_type,object_kind FROM blobs ORDER BY sha256"
        };
        let mut stmt = tx.prepare(blob_query)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            blob_count += 1;
            let hash: String = row.get(0)?;
            let path: String = row.get(1)?;
            let length: u64 = row.get(2)?;
            let media: String = row.get(3)?;
            let kind: String = row.get(4)?;
            let check = (|| -> Result<()> {
                let (extension, expected_media) = match kind.as_str() {
                    "png" => ("png", "image/png"),
                    segment::OBJECT_KIND => (segment::FILE_EXTENSION, segment::MEDIA_TYPE),
                    crate::image::reconstruction::OBJECT_KIND => (
                        crate::image::reconstruction::FILE_EXTENSION,
                        crate::image::reconstruction::MEDIA_TYPE,
                    ),
                    _ => return Err(integrity("Unknown blob object kind.")),
                };
                let bytes =
                    self.object_bytes(&hash, length, extension, self.limits.memory_bytes)?;
                if path != Self::object_path(&hash, extension) || media != expected_media {
                    return Err(integrity("Blob metadata mismatch."));
                }
                if kind == segment::OBJECT_KIND {
                    segment::decode_container(
                        &bytes,
                        &SegmentLimits {
                            max_bytes: self.limits.memory_bytes,
                            max_packets: 1024,
                            max_descriptor_bytes: 16 * 1024,
                        },
                    )?;
                    return Ok(());
                }
                if kind == crate::image::reconstruction::OBJECT_KIND {
                    ReconstructionMetadata::from_bytes(&bytes, &self.limits)?;
                    return Ok(());
                }
                let meta = image::validate(&bytes, &self.limits)?;
                let image_query = if self.format_version == 1 {
                    format!(
                        "{SELECT_IMAGE_V1} WHERE i.stored_blob_sha256=?1 OR i.source_blob_sha256=?1"
                    )
                } else {
                    format!(
                        "{SELECT_IMAGE_V2} WHERE r.png_blob_sha256=?1 OR rr.png_blob_sha256=?1 OR i.source_blob_sha256=?1"
                    )
                };
                let mut images = tx.prepare(&image_query)?;
                for r in images.query_map([&hash], row_image)? {
                    let r = r?;
                    if (r.width, r.height, r.bit_depth, r.color_type)
                        != (meta.width, meta.height, meta.bit_depth, meta.color_type)
                        || r.scanline_sha256 != meta.scanline_sha256
                        || r.non_idat_sha256 != meta.non_idat_sha256
                        || r.pixel_sha256 != meta.pixel_sha256
                    {
                        return Err(integrity("Image verification hashes or dimensions differ."));
                    }
                    if r.source_blob.as_ref() == Some(&hash)
                        && (r.source_sha256 != hash || r.source_bytes != length)
                    {
                        return Err(integrity("Source metadata differs."));
                    }
                }
                Ok(())
            })();
            if let Err(e) = check {
                record_issue(json!({"code":e.code,"blob_sha256":hash}), true)?;
            }
        }
        // Fixed-depth enumeration; never follow symlinks or scan outside objects.
        let objects = self.root.child("objects", false)?.child("sha256", false)?;
        for first in fs::read_dir(&objects.path)? {
            let first = first?;
            let a = first.file_name().to_string_lossy().into_owned();
            if !hex_prefix(&a) || !first.file_type()?.is_dir() {
                record_issue(json!({"code":"unexpected_object_entry"}), true)?;
                continue;
            }
            let d1 = objects.child(&a, false)?;
            for second in fs::read_dir(&d1.path)? {
                let second = second?;
                let b = second.file_name().to_string_lossy().into_owned();
                if !hex_prefix(&b) || !second.file_type()?.is_dir() {
                    record_issue(json!({"code":"unexpected_object_entry"}), true)?;
                    continue;
                }
                let d2 = d1.child(&b, false)?;
                for entry in fs::read_dir(&d2.path)? {
                    let entry = entry?;
                    let filename = entry.file_name().to_string_lossy().into_owned();
                    let hash = [".png", ".vpxs", ".pngr"]
                        .iter()
                        .find_map(|suffix| filename.strip_suffix(suffix))
                        .unwrap_or("");
                    if !entry.file_type()?.is_file()
                        || hash.len() != 64
                        || !hash.starts_with(&format!("{a}{b}"))
                        || !hash
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                    {
                        record_issue(json!({"code":"unexpected_object_entry"}), true)?;
                        continue;
                    }
                    let reference_query = if self.format_version == 1 {
                        "SELECT EXISTS(SELECT 1 FROM images WHERE stored_blob_sha256=?1 OR source_blob_sha256=?1)"
                    } else {
                        "SELECT EXISTS(SELECT 1 FROM images i LEFT JOIN representations r ON r.image_id=i.image_id LEFT JOIN retired_representations rr ON rr.image_id=i.image_id AND rr.prune_state!='deleted' LEFT JOIN frame_locations fl ON fl.image_id=i.image_id LEFT JOIN segments s ON s.segment_id=fl.segment_id LEFT JOIN png_reconstruction pr ON pr.image_id=i.image_id WHERE r.png_blob_sha256=?1 OR rr.png_blob_sha256=?1 OR i.source_blob_sha256=?1 OR s.color_blob_sha256=?1 OR s.alpha_blob_sha256=?1 OR pr.descriptor_blob_sha256=?1)"
                    };
                    let referenced: bool =
                        tx.query_row(reference_query, [hash], |row| row.get(0))?;
                    if !referenced {
                        record_issue(
                            json!({"code":"unreferenced_candidate","blob_sha256":hash}),
                            false,
                        )?;
                    }
                }
            }
        }
        let image_count: u64 = tx.query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))?;
        let summary = json!({"store_id":self.id,"images_checked":image_count,"blobs_checked":blob_count,"error_count":errors,"unreferenced_candidates":candidates,"issue_count":issue_count,"issues":examples,"valid":errors==0});
        if let Some(f) = &mut report_file {
            f.file.write_all(b"],\"summary\":")?;
            f.file.write_all(&serde_json::to_vec(&summary)?)?;
            f.file.write_all(b"}")?;
            let (dir, name) = destination.as_ref().unwrap();
            if !f.publish(dir, name)? {
                return Err(Error::new("E_OUTPUT_EXISTS", "Report already exists."));
            }
        }
        drop(rows);
        drop(stmt);
        tx.commit()?;
        Ok(summary)
    }
}
fn hex_prefix(s: &str) -> bool {
    s.len() == 2
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    store: Uuid,
    run: Option<String>,
    max: i64,
    last: i64,
}

#[cfg(test)]
mod tests {
    use super::LIST_RUN_SQL_V2;
    use rusqlite::{Connection, params};

    #[test]
    fn filtered_list_query_uses_run_sequence_index() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/002.sql"))
            .unwrap();
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {LIST_RUN_SQL_V2}"))
            .unwrap();
        let plan = statement
            .query_map(params!["rare", i64::MAX, i64::MAX, 21], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("images_by_run_seq") && detail.contains("run=?")),
            "query plan did not use the run/sequence index: {plan:?}"
        );
    }
}
