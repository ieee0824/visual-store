use crate::{
    Error, Result,
    error::{integrity, invalid},
    fault,
    filesystem::{self, Dir, Temp},
    image::{self, Limits},
    sha256,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path, time::Duration};
use uuid::Uuid;

const ENCODING: &str = "png-idat-zlib-v1";
// Reserve space for the CLI success envelope and trailing newline so list stdout
// stays within the documented 16 KiB budget.
const LIST_DATA_BUDGET_BYTES: usize = 16 * 1024 - 64;
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    store_id: Uuid,
    format_version: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PutOptions {
    pub run: Option<String>,
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
    fn fingerprint(&self, source: &str) -> Result<String> {
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
}

#[derive(Debug, Serialize)]
pub struct ImageRecord {
    pub image_id: String,
    pub seq: i64,
    pub run: Option<String>,
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
const SELECT_IMAGE: &str = "SELECT i.image_id,i.seq,i.run,i.created_at,i.captured_at,i.label,i.note,i.tags_json,i.width,i.height,i.bit_depth,i.color_type,i.source_sha256,i.source_byte_length,i.stored_blob_sha256,b.byte_length,i.source_blob_sha256,i.scanline_sha256,i.non_idat_sha256,i.pixel_sha256,i.encoding_version,i.compression_level,i.compression_applied FROM images i JOIN blobs b ON b.sha256=i.stored_blob_sha256";
fn row_image(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImageRecord> {
    let tags: String = r.get(7)?;
    let source_blob: Option<String> = r.get(16)?;
    Ok(ImageRecord {
        image_id: r.get(0)?,
        seq: r.get(1)?,
        run: r.get(2)?,
        created_at: r.get(3)?,
        captured_at: r.get(4)?,
        label: r.get(5)?,
        note: r.get(6)?,
        tags: serde_json::from_str(&tags).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?,
        width: r.get(8)?,
        height: r.get(9)?,
        bit_depth: r.get(10)?,
        color_type: r.get(11)?,
        source_sha256: r.get(12)?,
        source_bytes: r.get(13)?,
        stored_sha256: r.get(14)?,
        stored_bytes: r.get(15)?,
        source_retained: source_blob.is_some(),
        source_blob,
        scanline_sha256: r.get(17)?,
        non_idat_sha256: r.get(18)?,
        pixel_sha256: r.get(19)?,
        encoding_version: r.get(20)?,
        compression_level: r.get(21)?,
        compression_applied: r.get(22)?,
    })
}

pub struct Store {
    conn: Connection,
    root: Dir,
    id: Uuid,
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
    if m.format_version != 1 {
        return Err(Error::new("E_SCHEMA_VERSION", "Unsupported store format."));
    }
    Ok(m)
}
fn check_store(root: &Dir, c: &Connection, m: &Manifest) -> Result<()> {
    let v: u32 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if v != 1 {
        return Err(Error::new(
            "E_SCHEMA_VERSION",
            "Unsupported database schema.",
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

impl Store {
    pub fn initialize(path: &Path) -> Result<Value> {
        let root = Dir::create_root(path)?;
        root.lock(true)?;
        if fs::read_dir(&root.path)?.next().is_some() {
            let m = manifest(&root)?;
            let c = connect(&root, false)?;
            check_store(&root, &c, &m)?;
            return Ok(
                json!({"store_id":m.store_id,"schema_version":1,"already_initialized":true}),
            );
        }
        let m = Manifest {
            store_id: Uuid::new_v4(),
            format_version: 1,
        };
        root.child("objects", true)?.child("sha256", true)?;
        root.child("tmp", true)?;
        root.child("exports", true)?;
        root.new_file("index.sqlite3")?.sync_all()?;
        let mut c = connect(&root, true)?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(include_str!("../migrations/001.sql"))?;
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
        Ok(json!({"store_id":m.store_id,"schema_version":1,"already_initialized":false}))
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
        let m = manifest(&root)?;
        let conn = connect(&root, writable)?;
        check_store(&root, &conn, &m)?;
        Ok(Self {
            conn,
            root,
            id: m.store_id,
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
        self.conn
            .query_row(
                &format!("{SELECT_IMAGE} WHERE i.image_id=?1"),
                [id],
                row_image,
            )
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
        format!("objects/sha256/{}/{}/{}.png", &hash[..2], &hash[2..4], hash)
    }
    fn blob_bytes(&self, hash: &str, length: u64) -> Result<Vec<u8>> {
        let dir = self
            .blob_dir(hash, false)
            .map_err(|_| integrity("Blob directory missing or unsafe."))?;
        let mut f = dir
            .open_file(&format!("{hash}.png"))
            .map_err(|_| integrity("Blob missing or unsafe."))?;
        filesystem::check_hash(&mut f, length, hash, self.limits.source_bytes)
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
            let mut f = dir.open_file(&format!("{hash}.png"))?;
            filesystem::check_hash(&mut f, bytes.len() as u64, &hash, bytes.len())?;
            // The winning writer may not yet have synced the parent directory.
            dir.sync()?;
        }
        fault("after_blob_publish")?;
        Ok((hash, reused))
    }
    fn put_result(&self, id: &str, blob_reused: bool, record_reused: bool) -> Result<Value> {
        let r = self.record(id)?;
        Ok(
            json!({"ref":self.reference(&r.image_id),"image_id":r.image_id,"run":r.run,"seq":r.seq,
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
        let fingerprint = opts.fingerprint(&source_hash)?;
        if let Some(op) = &opts.operation_id {
            let prior: Option<(String, String)> = self
                .conn
                .query_row(
                    "SELECT image_id,operation_fingerprint FROM images WHERE operation_id=?1",
                    [op],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((id, fp)) = prior {
                if fp != fingerprint {
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
            let prior: Option<(String, String)> = tx
                .query_row(
                    "SELECT image_id,operation_fingerprint FROM images WHERE operation_id=?1",
                    [op],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((id, fp)) = prior {
                if fp != fingerprint {
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
            tx.execute("INSERT INTO blobs(sha256,relative_path,byte_length,media_type,created_at) VALUES (?1,?2,?3,'image/png',?4) ON CONFLICT(sha256) DO NOTHING",params![hash,relative,n as u64,created])?;
            let valid: bool = tx.query_row("SELECT relative_path=?2 AND byte_length=?3 AND media_type='image/png' FROM blobs WHERE sha256=?1",params![hash,relative,n as u64],|r|r.get(0))?;
            if !valid {
                return Err(integrity("Existing blob metadata differs."));
            }
        }
        let id = Uuid::new_v4().to_string();
        let m = &packed.meta;
        tx.execute("INSERT INTO images(image_id,run,created_at,captured_at,label,note,tags_json,width,height,bit_depth,color_type,source_sha256,source_byte_length,stored_blob_sha256,source_blob_sha256,scanline_sha256,non_idat_sha256,pixel_sha256,encoding_version,compression_level,compression_applied,operation_id,operation_fingerprint,validation_limits_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",params![id,opts.run,created,opts.captured_at,opts.label,opts.note,serde_json::to_string(&opts.tags)?,m.width,m.height,m.bit_depth,m.color_type,source_hash,bytes.len() as u64,stored_hash,source_blob,m.scanline_sha256,m.non_idat_sha256,m.pixel_sha256,ENCODING,opts.compression_level,packed.compression_applied,opts.operation_id,fingerprint,serde_json::to_string(&opts.limits)?])?;
        fault("during_db_commit")?;
        tx.commit()?;
        fault("after_db_commit")?;
        self.put_result(&id, reused, false)
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
        let mut stmt = self.conn.prepare("SELECT image_id,seq,run,label,width,height,created_at,(SELECT byte_length FROM blobs WHERE sha256=stored_blob_sha256) FROM images WHERE (?1 IS NULL OR run=?1) AND seq<=?2 AND seq<?3 ORDER BY seq DESC LIMIT ?4")?;
        let rows = stmt.query_map(params![run,c.max,c.last,limit+1],|r| Ok(json!({"ref":self.reference(&r.get::<_,String>(0)?),"seq":r.get::<_,i64>(1)?,"run":r.get::<_,Option<String>>(2)?,"label":r.get::<_,Option<String>>(3)?,"width":r.get::<_,u32>(4)?,"height":r.get::<_,u32>(5)?,"created_at":r.get::<_,String>(6)?,"stored_bytes":r.get::<_,u64>(7)?})))?;
        let candidates = rows.collect::<std::result::Result<Vec<_>, _>>()?;
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
        let mut stmt = tx.prepare(
            "SELECT sha256,relative_path,byte_length,media_type FROM blobs ORDER BY sha256",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            blob_count += 1;
            let hash: String = row.get(0)?;
            let path: String = row.get(1)?;
            let length: u64 = row.get(2)?;
            let media: String = row.get(3)?;
            let check = (|| -> Result<()> {
                let bytes = self.blob_bytes(&hash, length)?;
                if path != Self::blob_path(&hash) || media != "image/png" {
                    return Err(integrity("Blob metadata mismatch."));
                }
                let meta = image::validate(&bytes, &self.limits)?;
                let mut images = tx.prepare(&format!(
                    "{SELECT_IMAGE} WHERE i.stored_blob_sha256=?1 OR i.source_blob_sha256=?1"
                ))?;
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
                    let hash = filename.strip_suffix(".png").unwrap_or("");
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
                    let referenced: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM images WHERE stored_blob_sha256=?1 OR source_blob_sha256=?1)",[hash],|r|r.get(0))?;
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
