use clap::{Args, Parser, Subcommand, ValueEnum, error::ErrorKind};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::PathBuf,
};
use visual_store::{
    Error, PutOptions, Result, Store,
    image::Limits,
    store::{PackOptions, PruneOptions},
};

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
}
#[derive(Clone, Copy, ValueEnum)]
enum Variant {
    Stored,
    Source,
}
#[derive(Clone, Copy, ValueEnum)]
enum Codec {
    Vp9,
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
    let writable = matches!(cli.command, Command::Put { .. } | Command::Pack { .. });
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
            let code = if d["valid"] == false { 5 } else { 0 };
            return Ok((d, code));
        }
        Command::Pack {
            run,
            stream,
            codec: _,
            segment_frames,
            dry_run,
            max_segment_bytes,
            max_segment_packets,
            max_reconstruction_bytes,
            max_pack_images,
            max_encode_seconds,
        } => {
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
