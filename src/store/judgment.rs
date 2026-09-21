use super::{LIST_DATA_BUDGET_BYTES, Store, now};
use crate::{Error, Result, error::invalid};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{OptionalExtension, params, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

const JUDGMENT_FORMAT_VERSION: u32 = 3;
const MAX_JSON_BYTES: usize = 4 * 1024;
const MAX_TOTAL_JSON_BYTES: usize = 6 * 1024;

fn default_schema_version() -> u32 {
    1
}

fn empty_metadata() -> Value {
    json!({})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgmentInput {
    pub kind: String,
    pub producer: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub value: Value,
    #[serde(default)]
    pub probability: Option<f64>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default = "empty_metadata")]
    pub metadata: Value,
}

impl JudgmentInput {
    fn validate(&self) -> Result<(String, String)> {
        bounded_text(&self.kind, 128, "Kind")?;
        bounded_text(&self.producer, 128, "Producer")?;
        if let Some(model) = &self.model {
            bounded_text(model, 256, "Model")?;
        }
        if self.schema_version == 0 {
            return Err(invalid("Judgment schema version must be positive."));
        }
        for (name, number) in [
            ("Probability", self.probability),
            ("Confidence", self.confidence),
        ] {
            if number.is_some_and(|number| !(0.0..=1.0).contains(&number)) {
                return Err(invalid(&format!("{name} must be between 0 and 1.")));
            }
        }
        if !self.metadata.is_object() {
            return Err(invalid("Judgment metadata must be a JSON object."));
        }
        let value_json = canonical_json(&self.value, "Judgment value")?;
        let metadata_json = canonical_json(&self.metadata, "Judgment metadata")?;
        if value_json.len() + metadata_json.len() > MAX_TOTAL_JSON_BYTES {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Combined judgment value and metadata exceed the 6 KiB limit.",
            ));
        }
        Ok((value_json, metadata_json))
    }
}

#[derive(Debug, Clone, Default)]
pub struct JudgmentFilter {
    pub kind: Option<String>,
    pub producer: Option<String>,
    pub value: Option<Value>,
    pub confidence_below: Option<f64>,
}

impl JudgmentFilter {
    fn normalize(&self) -> Result<NormalizedFilter> {
        if let Some(kind) = &self.kind {
            bounded_text(kind, 128, "Kind")?;
        }
        if let Some(producer) = &self.producer {
            bounded_text(producer, 128, "Producer")?;
        }
        if self
            .confidence_below
            .is_some_and(|number| !(0.0..=1.0).contains(&number))
        {
            return Err(invalid("Confidence threshold must be between 0 and 1."));
        }
        Ok(NormalizedFilter {
            kind: self.kind.clone(),
            producer: self.producer.clone(),
            value_json: self
                .value
                .as_ref()
                .map(|value| canonical_json(value, "Judgment value"))
                .transpose()?,
            confidence_below: self.confidence_below,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct NormalizedFilter {
    kind: Option<String>,
    producer: Option<String>,
    value_json: Option<String>,
    confidence_below: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgmentCursor {
    version: u8,
    store: Uuid,
    image_id: Option<String>,
    filter: NormalizedFilter,
    max: i64,
    last: i64,
}

fn bounded_text(value: &str, max: usize, label: &str) -> Result<()> {
    if value.is_empty() {
        return Err(invalid(&format!("{label} must not be empty.")));
    }
    if value.len() > max {
        return Err(Error::new(
            "E_LIMIT_EXCEEDED",
            format!("{label} exceeds the {max}-byte limit."),
        ));
    }
    Ok(())
}

fn canonical_json(value: &Value, label: &str) -> Result<String> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| invalid(&format!("{label} is not valid JSON.")))?;
    if encoded.len() > MAX_JSON_BYTES {
        return Err(Error::new(
            "E_LIMIT_EXCEEDED",
            format!("{label} exceeds the {MAX_JSON_BYTES}-byte limit."),
        ));
    }
    Ok(encoded)
}

fn parse_json(value: String, column: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

impl Store {
    fn require_judgments(&self) -> Result<()> {
        if self.format_version < JUDGMENT_FORMAT_VERSION {
            return Err(Error::new(
                "E_SCHEMA_VERSION",
                "Judgments require a version 3 store; run migrate --to 3.",
            ));
        }
        Ok(())
    }

    pub fn add_judgment(&self, reference: &str, input: JudgmentInput) -> Result<Value> {
        self.require_judgments()?;
        let image_id = self.resolve(reference)?;
        self.record(&image_id)?;
        let (value_json, metadata_json) = input.validate()?;
        let judgment_id = Uuid::new_v4().to_string();
        let created_at = now();
        self.conn.execute(
            "INSERT INTO judgments(judgment_id,image_id,kind,producer,model,schema_version,value_json,probability,confidence,metadata_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                judgment_id,
                image_id,
                input.kind,
                input.producer,
                input.model,
                input.schema_version,
                value_json,
                input.probability,
                input.confidence,
                metadata_json,
                created_at,
            ],
        )?;
        Ok(json!({
            "judgment_id": judgment_id,
            "ref": self.reference(&image_id),
            "image_id": image_id,
            "kind": input.kind,
            "producer": input.producer,
            "model": input.model,
            "schema_version": input.schema_version,
            "value": input.value,
            "probability": input.probability,
            "confidence": input.confidence,
            "metadata": input.metadata,
            "created_at": created_at,
        }))
    }

    pub fn list_judgments(
        &self,
        reference: &str,
        filter: JudgmentFilter,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Value> {
        self.require_judgments()?;
        let image_id = self.resolve(reference)?;
        self.record(&image_id)?;
        self.query_judgments(Some(image_id), filter, limit, cursor)
    }

    pub fn search_judgments(
        &self,
        filter: JudgmentFilter,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Value> {
        self.require_judgments()?;
        self.query_judgments(None, filter, limit, cursor)
    }

    fn query_judgments(
        &self,
        image_id: Option<String>,
        filter: JudgmentFilter,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Value> {
        if !(1..=100).contains(&limit) {
            return Err(invalid("Limit must be 1 through 100."));
        }
        let filter = filter.normalize()?;
        let invalid_cursor = || {
            Error::new(
                "E_INVALID_CURSOR",
                "Cursor is invalid or belongs to another judgment query.",
            )
        };
        let position = if let Some(raw) = cursor {
            if raw.len() > 4096 {
                return Err(invalid_cursor());
            }
            let decoded = URL_SAFE_NO_PAD.decode(raw).map_err(|_| invalid_cursor())?;
            let decoded: JudgmentCursor =
                serde_json::from_slice(&decoded).map_err(|_| invalid_cursor())?;
            if decoded.version != 1
                || decoded.store != self.id
                || decoded.image_id != image_id
                || decoded.filter != filter
                || decoded.last <= 0
                || decoded.last > decoded.max
            {
                return Err(invalid_cursor());
            }
            decoded
        } else {
            JudgmentCursor {
                version: 1,
                store: self.id,
                image_id: image_id.clone(),
                filter: filter.clone(),
                max: self.conn.query_row(
                    "SELECT COALESCE(MAX(seq),0) FROM judgments",
                    [],
                    |row| row.get(0),
                )?,
                last: i64::MAX,
            }
        };

        let mut sql = String::from(
            "SELECT j.seq,j.judgment_id,j.image_id,j.kind,j.producer,j.model,j.schema_version,j.value_json,j.probability,j.confidence,j.metadata_json,j.created_at FROM judgments j WHERE j.seq<=? AND j.seq<?",
        );
        let mut parameters = vec![
            SqlValue::Integer(position.max),
            SqlValue::Integer(position.last),
        ];
        if let Some(image_id) = &image_id {
            sql.push_str(" AND j.image_id=?");
            parameters.push(SqlValue::Text(image_id.clone()));
        }
        if let Some(kind) = &filter.kind {
            sql.push_str(" AND j.kind=?");
            parameters.push(SqlValue::Text(kind.clone()));
        }
        if let Some(producer) = &filter.producer {
            sql.push_str(" AND j.producer=?");
            parameters.push(SqlValue::Text(producer.clone()));
        }
        if let Some(value_json) = &filter.value_json {
            sql.push_str(" AND j.value_json=?");
            parameters.push(SqlValue::Text(value_json.clone()));
        }
        if let Some(confidence) = filter.confidence_below {
            sql.push_str(" AND j.confidence<?");
            parameters.push(SqlValue::Real(confidence));
        }
        sql.push_str(" ORDER BY j.seq DESC LIMIT ?");
        parameters.push(SqlValue::Integer(i64::from(limit) + 1));

        let mut statement = self.conn.prepare(&sql)?;
        let candidates = statement
            .query_map(params_from_iter(parameters), |row| {
                let image_id: String = row.get(2)?;
                Ok(json!({
                    "seq": row.get::<_, i64>(0)?,
                    "judgment_id": row.get::<_, String>(1)?,
                    "ref": self.reference(&image_id),
                    "image_id": image_id,
                    "kind": row.get::<_, String>(3)?,
                    "producer": row.get::<_, String>(4)?,
                    "model": row.get::<_, Option<String>>(5)?,
                    "schema_version": row.get::<_, u32>(6)?,
                    "value": parse_json(row.get(7)?, 7)?,
                    "probability": row.get::<_, Option<f64>>(8)?,
                    "confidence": row.get::<_, Option<f64>>(9)?,
                    "metadata": parse_json(row.get(10)?, 10)?,
                    "created_at": row.get::<_, String>(11)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let page = |items: &[Value], more: bool| -> Result<Value> {
            let mut result = json!({"items": items});
            if more {
                let last = items
                    .last()
                    .and_then(|item| item["seq"].as_i64())
                    .ok_or_else(&invalid_cursor)?;
                result["next_cursor"] = URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&JudgmentCursor {
                        version: 1,
                        store: self.id,
                        image_id: image_id.clone(),
                        filter: filter.clone(),
                        max: position.max,
                        last,
                    })?)
                    .into();
            }
            Ok(result)
        };
        let mut items = Vec::with_capacity((limit as usize).min(candidates.len()));
        for item in candidates.iter().take(limit as usize) {
            items.push(item.clone());
            if serde_json::to_vec(&page(&items, items.len() < candidates.len())?)?.len()
                > LIST_DATA_BUDGET_BYTES
            {
                items.pop();
                if items.is_empty() {
                    return Err(Error::new(
                        "E_LIMIT_EXCEEDED",
                        "A judgment exceeds the output byte budget.",
                    ));
                }
                break;
            }
        }
        page(&items, items.len() < candidates.len())
    }

    pub fn lightweight_features(&self, reference: &str) -> Result<Value> {
        let current = self.record(&self.resolve(reference)?)?;
        let previous = match (&current.run, &current.stream, current.frame_no) {
            (Some(run), Some(stream), Some(frame_no)) if frame_no > 0 => self
                .conn
                .query_row(
                    "SELECT image_id FROM images WHERE run=?1 AND stream=?2 AND frame_no=?3",
                    params![run, stream, frame_no - 1],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|id| self.record(&id))
                .transpose()?,
            _ => None,
        };
        let previous = previous.map(|previous| {
            json!({
                "ref": self.reference(&previous.image_id),
                "image_id": previous.image_id,
                "pixel_sha256": previous.pixel_sha256,
                "same_pixels": previous.pixel_sha256 == current.pixel_sha256,
            })
        });
        Ok(json!({
            "ref": self.reference(&current.image_id),
            "image_id": current.image_id,
            "run": current.run,
            "stream": current.stream,
            "frame_no": current.frame_no,
            "sha256": {
                "source": current.source_sha256,
                "pixels": current.pixel_sha256,
                "stored_png": current.stored_sha256,
            },
            "width": current.width,
            "height": current.height,
            "source_bytes": current.source_bytes,
            "representation_bytes": current.shared_representation_bytes,
            "representation_bytes_semantics": "shared_and_not_additive_across_images",
            "previous": previous,
        }))
    }
}
