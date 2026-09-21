use super::{
    Manifest, MigrationJournal, MigrationState, connect, database_version, manifest,
    migration_journal, now, write_json_atomic, write_manifest,
};
use crate::{
    Error, Result,
    error::{integrity, invalid},
    fault,
    filesystem::{self, Dir},
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, backup::Backup, params,
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::MetadataExt, path::Path, time::Duration};
use uuid::Uuid;

const FROM_VERSION: u32 = 2;
const TO_VERSION: u32 = 3;
const JOURNAL: &str = "migration-v2-to-v3.json";
const BACKUP: &str = "migration-v2-backup.sqlite3";
const BACKUP_FILES: [&str; 4] = [
    "migration-v2-backup.sqlite3-wal",
    "migration-v2-backup.sqlite3-shm",
    "migration-v2-backup.sqlite3-journal",
    BACKUP,
];

fn incomplete() -> Error {
    Error::new(
        "E_MIGRATION_INCOMPLETE",
        "Store migration is incomplete; run migrate --to 3 --resume or --restore.",
    )
}

fn read_journal(root: &Dir) -> Result<Option<MigrationJournal>> {
    let mut file = match root.open_file(JOURNAL) {
        Ok(file) => file,
        Err(error)
            if error.code == "E_IO" && !root.path.join(JOURNAL).try_exists().unwrap_or(true) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let journal: MigrationJournal =
        serde_json::from_slice(&filesystem::read_bounded(&mut file, 4096)?)?;
    if journal.journal_version != 1
        || journal.from_version != FROM_VERSION
        || journal.to_version != TO_VERSION
        || journal.backup_file != BACKUP
    {
        return Err(integrity("Invalid version-3 migration journal."));
    }
    Ok(Some(journal))
}

pub(super) fn journal_exists(root: &Dir) -> Result<bool> {
    Ok(read_journal(root)?.is_some())
}

fn write_journal(root: &Dir, journal: &MigrationJournal) -> Result<()> {
    write_json_atomic(root, JOURNAL, journal)
}

fn remove_backup(root: &Dir) -> Result<()> {
    for name in BACKUP_FILES {
        root.remove_file_if_exists(name)?;
    }
    Ok(())
}

fn validate_backup(path: &Path, expected_store_id: Uuid) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(integrity("Unsafe version-3 migration backup path."));
    }
    let backup = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    if database_version(&backup)? != FROM_VERSION {
        return Err(integrity(
            "Version-3 migration backup has an unexpected schema version.",
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
    let check: String = backup.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
    if check != "ok" {
        return Err(integrity("Migration backup failed SQLite integrity check."));
    }
    Ok(())
}

fn create_backup(root: &Dir, source: &Connection, store_id: Uuid) -> Result<()> {
    remove_backup(root)?;
    drop(root.new_file(BACKUP)?);
    let mut destination = Connection::open_with_flags(
        root.path.join(BACKUP),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Backup::new(source, &mut destination)?.run_to_completion(
        100,
        Duration::from_millis(10),
        None,
    )?;
    drop(destination);
    root.open_file(BACKUP)?.sync_all()?;
    root.sync()?;
    validate_backup(&root.path.join(BACKUP), store_id)
}

fn apply_schema(connection: &mut Connection, journal: &MigrationJournal) -> Result<()> {
    connection.pragma_update(None, "foreign_keys", false)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(include_str!("../../migrations/003.sql"))?;
    transaction.execute(
        "INSERT INTO schema_migrations(from_version,to_version,started_at,completed_at,backup_file) VALUES (2,3,?1,NULL,?2)",
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

fn restore(
    root: &Dir,
    connection: &mut Connection,
    manifest_value: &mut Manifest,
    journal: &mut MigrationJournal,
) -> Result<Value> {
    let restored_from_version = database_version(connection)?;
    validate_backup(&root.path.join(BACKUP), journal.store_id)?;
    journal.state = MigrationState::Restoring;
    write_journal(root, journal)?;
    fault("migration_v3_restore_started")?;
    let backup = Connection::open_with_flags(
        root.path.join(BACKUP),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Backup::new(&backup, connection)?.run_to_completion(100, Duration::from_millis(10), None)?;
    drop(backup);
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    fault("migration_v3_restore_after_db")?;
    manifest_value.format_version = FROM_VERSION;
    write_manifest(root, manifest_value)?;
    fault("migration_v3_restore_after_manifest")?;
    root.remove_file_if_exists(JOURNAL)?;
    fault("migration_v3_restore_after_journal_cleanup")?;
    remove_backup(root)?;
    Ok(json!({
        "store_id": journal.store_id,
        "from_version": restored_from_version,
        "to_version": FROM_VERSION,
        "migrated": false,
        "resumed": true,
        "restored": true,
    }))
}

pub(super) fn migrate(path: &Path, resume: bool, restore_requested: bool) -> Result<Value> {
    if resume && restore_requested {
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
    if migration_journal(&root)?.is_some() {
        return Err(Error::new(
            "E_MIGRATION_INCOMPLETE",
            "Complete the version-2 migration before migrating to version 3.",
        ));
    }
    root.check_sqlite_paths()?;
    let mut manifest_value = manifest(&root)?;
    let mut connection = connect(&root, true)?;
    let initial_version = database_version(&connection)?;
    let mut journal = read_journal(&root)?;

    if journal.is_some() && !resume && !restore_requested {
        return Err(incomplete());
    }
    if restore_requested && journal.is_none() {
        if manifest_value.format_version == FROM_VERSION
            && initial_version == FROM_VERSION
            && root.path.join(BACKUP).try_exists()?
        {
            validate_backup(&root.path.join(BACKUP), manifest_value.store_id)?;
            remove_backup(&root)?;
            return Ok(json!({
                "store_id": manifest_value.store_id,
                "from_version": FROM_VERSION,
                "to_version": FROM_VERSION,
                "migrated": false,
                "resumed": true,
                "restored": true,
            }));
        }
        return Err(invalid("There is no incomplete migration to restore."));
    }
    if journal.is_none()
        && manifest_value.format_version == TO_VERSION
        && initial_version == TO_VERSION
    {
        return Ok(json!({
            "store_id": manifest_value.store_id,
            "from_version": TO_VERSION,
            "to_version": TO_VERSION,
            "migrated": false,
            "resumed": false,
            "restored": false,
        }));
    }
    if journal.is_none()
        && (manifest_value.format_version != FROM_VERSION || initial_version != FROM_VERSION)
    {
        return Err(Error::new(
            "E_SCHEMA_VERSION",
            "Migration to version 3 requires a consistent version 2 store.",
        ));
    }

    if journal.is_none() {
        create_backup(&root, &connection, manifest_value.store_id)?;
        fault("migration_v3_after_backup")?;
        let started = MigrationJournal {
            journal_version: 1,
            store_id: manifest_value.store_id,
            from_version: FROM_VERSION,
            to_version: TO_VERSION,
            state: MigrationState::Started,
            backup_file: BACKUP.into(),
            started_at: now(),
        };
        write_journal(&root, &started)?;
        fault("migration_v3_after_journal")?;
        journal = Some(started);
    }

    let mut journal = journal.expect("migration journal was initialized");
    if journal.store_id != manifest_value.store_id {
        return Err(integrity(
            "Migration journal store ID differs from manifest.",
        ));
    }
    if restore_requested {
        return restore(&root, &mut connection, &mut manifest_value, &mut journal);
    }

    match database_version(&connection)? {
        FROM_VERSION => {
            if manifest_value.format_version != FROM_VERSION {
                return Err(incomplete());
            }
            validate_backup(&root.path.join(BACKUP), journal.store_id)?;
            fault("migration_v3_before_db_commit")?;
            apply_schema(&mut connection, &journal)?;
            fault("migration_v3_after_db_commit")?;
        }
        TO_VERSION => {}
        _ => return Err(incomplete()),
    }
    journal.state = MigrationState::DbCommitted;
    write_journal(&root, &journal)?;
    fault("migration_v3_after_db_journal")?;

    match manifest_value.format_version {
        FROM_VERSION => {
            manifest_value.format_version = TO_VERSION;
            write_manifest(&root, &manifest_value)?;
            fault("migration_v3_after_manifest")?;
        }
        TO_VERSION => {}
        _ => return Err(incomplete()),
    }
    journal.state = MigrationState::ManifestUpdated;
    write_journal(&root, &journal)?;
    fault("migration_v3_after_manifest_journal")?;

    let completion: Option<Option<String>> = connection
        .query_row(
            "SELECT completed_at FROM schema_migrations WHERE from_version=2 AND to_version=3 ORDER BY migration_id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match completion {
        Some(None) => {
            connection.execute(
                "UPDATE schema_migrations SET completed_at=?1 WHERE migration_id=(SELECT MAX(migration_id) FROM schema_migrations WHERE from_version=2 AND to_version=3)",
                [now()],
            )?;
        }
        Some(Some(_)) => {}
        None => return Err(integrity("Migration history is missing.")),
    }
    fault("migration_v3_before_cleanup")?;
    remove_backup(&root)?;
    fault("migration_v3_after_backup_cleanup")?;
    root.remove_file_if_exists(JOURNAL)?;
    fault("migration_v3_after_cleanup")?;
    Ok(json!({
        "store_id": manifest_value.store_id,
        "from_version": FROM_VERSION,
        "to_version": TO_VERSION,
        "migrated": true,
        "resumed": resume,
        "restored": false,
    }))
}
