use clap::{Args, Parser, Subcommand, ValueEnum, error::ErrorKind};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
};
use visual_store::{
    Error, PutOptions, Result, Store,
    image::Limits,
    store::{JudgmentFilter, JudgmentInput, PackOptions, PruneOptions},
};

const MAX_JUDGMENT_INPUT_BYTES: u64 = 64 * 1024;

#[derive(Parser)]
#[command(
    name = "vstore",
    version,
    about = "Local PNG storage; returns metadata, never image bytes"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "VSTORE_ROOT",
        default_value = ".visual-store"
    )]
    store: PathBuf,
    #[command(flatten)]
    limits: LimitArgs,
    #[command(subcommand)]
    command: Command,
}
#[derive(Args)]
struct LimitArgs {
    #[arg(long, global = true, default_value_t = 67108864)]
    max_source_bytes: usize,
    #[arg(long, global = true, default_value_t = 16384)]
    max_edge: u32,
    #[arg(long, global = true, default_value_t = 16777216)]
    max_pixels: u64,
    #[arg(long, global = true, default_value_t = 134217728)]
    max_inflated_bytes: usize,
    #[arg(long, global = true, default_value_t = 268435456)]
    max_memory_bytes: usize,
}
impl From<LimitArgs> for Limits {
    fn from(a: LimitArgs) -> Self {
        Self {
            source_bytes: a.max_source_bytes,
            max_edge: a.max_edge,
            pixels: a.max_pixels,
            inflated_bytes: a.max_inflated_bytes,
            memory_bytes: a.max_memory_bytes,
        }
    }
}
#[derive(Subcommand)]
enum Command {
    Init,
    Put {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        stream: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        captured_at: Option<String>,
        #[arg(long)]
        keep_source: bool,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long,default_value_t=6,value_parser=clap::value_parser!(u32).range(0..=9))]
        compression_level: u32,
    },
    Info {
        reference: String,
    },
    Features {
        reference: String,
    },
    List {
        #[arg(long)]
        run: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
    Get {
        reference: String,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long,value_enum,default_value_t=Variant::Stored)]
        variant: Variant,
    },
    GetFrame {
        #[arg(long)]
        run: String,
        #[arg(long, default_value = "default")]
        stream: String,
        #[arg(long)]
        frame: u64,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long,value_enum,default_value_t=Variant::Stored)]
        variant: Variant,
    },
    Verify {
        #[arg(long)]
        report: Option<PathBuf>,
    },
    Pack {
        #[arg(long)]
        run: String,
        #[arg(long)]
        stream: Option<String>,
        #[arg(long, value_enum, default_value_t = Codec::Vp9)]
        codec: Codec,
        #[arg(long, default_value_t = 32)]
        segment_frames: usize,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value_t = 134217728)]
        max_segment_bytes: usize,
        #[arg(long, default_value_t = 1024)]
        max_segment_packets: usize,
        #[arg(long, default_value_t = 67108864)]
        max_reconstruction_bytes: usize,
        #[arg(long, default_value_t = 10000)]
        max_pack_images: usize,
        #[arg(long, default_value_t = 300)]
        max_encode_seconds: u64,
    },
    Prune {
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,
        #[arg(long, default_value_t = 10000)]
        max_prune_objects: usize,
    },
    Migrate {
        #[arg(long)]
        to: u32,
        #[arg(long, conflicts_with = "restore")]
        resume: bool,
        #[arg(long, conflicts_with = "resume")]
        restore: bool,
    },
    Judgment {
        #[command(subcommand)]
        command: JudgmentCommand,
    },
}

#[derive(Subcommand)]
enum JudgmentCommand {
    Add {
        reference: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        producer: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        schema_version: Option<u32>,
        #[arg(long)]
        value: Option<String>,
        #[arg(long)]
        probability: Option<f64>,
        #[arg(long)]
        confidence: Option<f64>,
        #[arg(long)]
        metadata: Option<String>,
        #[arg(long = "json", conflicts_with = "stdin")]
        json_file: Option<PathBuf>,
        #[arg(long, conflicts_with = "json_file")]
        stdin: bool,
    },
    List {
        reference: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        producer: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
    Search {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        producer: Option<String>,
        #[arg(long)]
        value: Option<String>,
        #[arg(long)]
        confidence_below: Option<f64>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        cursor: Option<String>,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum Variant {
    Stored,
    Source,
}
#[derive(Clone, Copy, ValueEnum)]
enum Codec {
    Vp9,
    Av1,
}

fn user_json(raw: &str, label: &str) -> Result<Value> {
    serde_json::from_str(raw)
        .map_err(|_| Error::new("E_INVALID_ARGUMENT", format!("{label} must be valid JSON.")))
}

fn judgment_from_reader(mut reader: impl Read) -> Result<JudgmentInput> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(MAX_JUDGMENT_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JUDGMENT_INPUT_BYTES {
        return Err(Error::new(
            "E_LIMIT_EXCEEDED",
            "Judgment JSON exceeds the 64 KiB input limit.",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        Error::new(
            "E_INVALID_ARGUMENT",
            "Judgment input must match the documented JSON object.",
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn judgment_input(
    kind: Option<String>,
    producer: Option<String>,
    model: Option<String>,
    schema_version: Option<u32>,
    value: Option<String>,
    probability: Option<f64>,
    confidence: Option<f64>,
    metadata: Option<String>,
    json_file: Option<PathBuf>,
    stdin: bool,
) -> Result<JudgmentInput> {
    let has_inline = kind.is_some()
        || producer.is_some()
        || model.is_some()
        || schema_version.is_some()
        || value.is_some()
        || probability.is_some()
        || confidence.is_some()
        || metadata.is_some();
    if let Some(path) = json_file {
        if has_inline {
            return Err(Error::new(
                "E_INVALID_ARGUMENT",
                "Use either --json or inline judgment fields, not both.",
            ));
        }
        return judgment_from_reader(File::open(path)?);
    }
    if stdin {
        if has_inline {
            return Err(Error::new(
                "E_INVALID_ARGUMENT",
                "Use either --stdin or inline judgment fields, not both.",
            ));
        }
        return judgment_from_reader(io::stdin().lock());
    }
    let kind = kind.ok_or_else(|| {
        Error::new(
            "E_INVALID_ARGUMENT",
            "Inline judgment input requires --kind.",
        )
    })?;
    let producer = producer.ok_or_else(|| {
        Error::new(
            "E_INVALID_ARGUMENT",
            "Inline judgment input requires --producer.",
        )
    })?;
    let value = value.ok_or_else(|| {
        Error::new(
            "E_INVALID_ARGUMENT",
            "Inline judgment input requires --value JSON.",
        )
    })?;
    Ok(JudgmentInput {
        kind,
        producer,
        model,
        schema_version: schema_version.unwrap_or(1),
        value: user_json(&value, "Judgment value")?,
        probability,
        confidence,
        metadata: metadata
            .as_deref()
            .map(|raw| user_json(raw, "Judgment metadata"))
            .transpose()?
            .unwrap_or_else(|| json!({})),
    })
}

fn run(cli: Cli) -> Result<(Value, i32)> {
    let limits: Limits = cli.limits.into();
    limits.validate()?;
    if matches!(cli.command, Command::Init) {
        return Ok((Store::initialize(&cli.store)?, 0));
    }
    if let Command::Migrate {
        to,
        resume,
        restore,
    } = &cli.command
    {
        return Ok((Store::migrate(&cli.store, *to, *resume, *restore)?, 0));
    }
    if let Command::Prune {
        dry_run,
        apply,
        max_prune_objects,
    } = &cli.command
    {
        let outcome = Store::prune(
            &cli.store,
            PruneOptions {
                dry_run: *dry_run,
                apply: *apply,
                max_objects: *max_prune_objects,
            },
            limits,
        )?;
        if let Some(error) = outcome.error {
            return Ok((
                json!({"prune":outcome.data,"operation_error":error}),
                error.exit_code(),
            ));
        }
        return Ok((outcome.data, 0));
    }
    let writable = matches!(
        &cli.command,
        Command::Put { .. }
            | Command::Pack { .. }
            | Command::Judgment {
                command: JudgmentCommand::Add { .. }
            }
    );
    let mut store = Store::open(&cli.store, writable)?;
    store.limits = limits.clone();
    let data = match cli.command {
        Command::Init => unreachable!(),
        Command::Put {
            file,
            run,
            stream,
            label,
            note,
            tags,
            captured_at,
            keep_source,
            operation_id,
            compression_level,
        } => store.put(
            &file,
            PutOptions {
                run,
                stream,
                label,
                note,
                tags,
                captured_at,
                keep_source,
                operation_id,
                compression_level,
                limits,
            },
        )?,
        Command::Info { reference } => store.info(&reference)?,
        Command::Features { reference } => store.lightweight_features(&reference)?,
        Command::List { run, limit, cursor } => store.list(run, limit, cursor.as_deref())?,
        Command::Get {
            reference,
            output,
            variant,
        } => store.materialize(
            &reference,
            matches!(variant, Variant::Source),
            output.as_deref(),
        )?,
        Command::GetFrame {
            run,
            stream,
            frame,
            output,
            variant,
        } => {
            let id = store.resolve_frame(&run, &stream, frame)?;
            store.materialize(&id, matches!(variant, Variant::Source), output.as_deref())?
        }
        Command::Verify { report } => {
            let d = store.verify(report.as_deref())?;
            if d["valid"] == false {
                let unavailable = d["issues"].as_array().is_some_and(|issues| {
                    issues
                        .iter()
                        .filter(|issue| issue["code"] == "E_CODEC_UNAVAILABLE")
                        .count() as u64
                        == d["error_count"].as_u64().unwrap_or(u64::MAX)
                });
                let error = if unavailable {
                    Error::new(
                        "E_CODEC_UNAVAILABLE",
                        "Temporal segments could not be fully verified because VP9 support is unavailable.",
                    )
                } else {
                    Error::new(
                        "E_INTEGRITY",
                        "Store verification found integrity problems.",
                    )
                };
                return Ok((
                    json!({"report":d,"operation_error":error}),
                    error.exit_code(),
                ));
            }
            return Ok((d, 0));
        }
        Command::Pack {
            run,
            stream,
            codec,
            segment_frames,
            dry_run,
            max_segment_bytes,
            max_segment_packets,
            max_reconstruction_bytes,
            max_pack_images,
            max_encode_seconds,
        } => {
            if matches!(codec, Codec::Av1) {
                return Err(Error::new(
                    "E_CODEC_UNAVAILABLE",
                    "The AV1 backend is not implemented; use --codec vp9.",
                ));
            }
            let outcome = store.pack(PackOptions {
                run,
                stream,
                segment_frames,
                dry_run,
                max_segment_bytes,
                max_segment_packets,
                max_reconstruction_bytes,
                max_pack_images,
                max_encode_seconds,
                limits,
            })?;
            if let Some(error) = outcome.error {
                return Ok((
                    json!({"pack":outcome.data,"pack_error":error}),
                    error.exit_code(),
                ));
            }
            outcome.data
        }
        Command::Migrate { .. } => unreachable!(),
        Command::Prune { .. } => unreachable!(),
        Command::Judgment { command } => match command {
            JudgmentCommand::Add {
                reference,
                kind,
                producer,
                model,
                schema_version,
                value,
                probability,
                confidence,
                metadata,
                json_file,
                stdin,
            } => store.add_judgment(
                &reference,
                judgment_input(
                    kind,
                    producer,
                    model,
                    schema_version,
                    value,
                    probability,
                    confidence,
                    metadata,
                    json_file,
                    stdin,
                )?,
            )?,
            JudgmentCommand::List {
                reference,
                kind,
                producer,
                limit,
                cursor,
            } => store.list_judgments(
                &reference,
                JudgmentFilter {
                    kind,
                    producer,
                    ..JudgmentFilter::default()
                },
                limit,
                cursor.as_deref(),
            )?,
            JudgmentCommand::Search {
                kind,
                producer,
                value,
                confidence_below,
                limit,
                cursor,
            } => store.search_judgments(
                JudgmentFilter {
                    kind,
                    producer,
                    value: value
                        .as_deref()
                        .map(|raw| user_json(raw, "Judgment value"))
                        .transpose()?,
                    confidence_below,
                },
                limit,
                cursor.as_deref(),
            )?,
        },
    };
    Ok((data, 0))
}
fn emit(value: Value, code: i32) -> ! {
    let mut out = io::stdout().lock();
    if serde_json::to_writer(&mut out, &value).is_err()
        || out.write_all(b"\n").is_err()
        || out.flush().is_err()
    {
        std::process::exit(6);
    }
    std::process::exit(code)
}
fn fail(error: Error) -> ! {
    let code = error.exit_code();
    emit(json!({"schema_version":2,"ok":false,"error":error}), code)
}
fn main() {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            print!("{e}");
            return;
        }
        Err(_) => fail(Error::new(
            "E_INVALID_ARGUMENT",
            "Invalid command arguments. Use --help for usage.",
        )),
    };
    match run(cli) {
        Ok((data, 0)) => emit(json!({"schema_version":2,"ok":true,"data":data}), 0),
        Ok((data, code)) => {
            let error = data
                .get("pack_error")
                .or_else(|| data.get("operation_error"))
                .cloned()
                .unwrap_or_else(|| {
                    serde_json::to_value(Error::new(
                        "E_INTEGRITY",
                        "Store verification found integrity problems.",
                    ))
                    .unwrap()
                });
            let data = data
                .get("pack")
                .or_else(|| data.get("prune"))
                .or_else(|| data.get("report"))
                .cloned()
                .unwrap_or(data);
            emit(
                json!({"schema_version":2,"ok":false,"data":data,"error":error}),
                code,
            )
        }
        Err(e) => fail(e),
    }
}
