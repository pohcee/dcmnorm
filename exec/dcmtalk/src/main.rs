use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand};
use dcmnorm::dicom_io::{
    echo_scu, find_scu, move_scu, start_scp, store_scu, DimseLogger, EchoScuOptions,
    FindScuOptions, MoveScuOptions, MoveScuResult, ScpHandlers, ScpOptions, StoreScuOptions,
    StoreScuResult,
};

/// Classic DICOM default proposed/accepted PDU length, matching dcmtk's SCU tools' own default.
const DEFAULT_SCU_MAX_PDU: u32 = 16_384;
/// Matches `ScpOptions::default()`'s own max PDU length.
const DEFAULT_SCP_MAX_PDU: u32 = 262_144;
const DEFAULT_SCP_IDLE_TIMEOUT_SECS: u64 = 300;
const DEFAULT_AE_TITLE: &str = "DCMTALK";

#[derive(Parser)]
#[command(name = "dcmtalk")]
#[command(version)]
#[command(about = "DICOM network (DIMSE) client/server: verify, send, query, retrieve, and receive")]
#[command(long_about = "A DIMSE command-line tool covering the same ground as dcmtk's echoscu/storescu/findscu/movescu/storescp, built on dcmnorm's native (non-dcmtk) DICOM Upper Layer implementation. Every subcommand accepts --verbose to log association negotiation, presentation contexts, and each DIMSE command/response exchanged.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// C-ECHO SCU: verify connectivity to a DICOM peer
    Echoscu(EchoscuArgs),
    /// C-STORE SCU: send DICOM file(s)/director(ies) to a peer
    Storescu(StorescuArgs),
    /// C-FIND SCU: query a peer's studies (Study Root Query/Retrieve)
    Findscu(FindscuArgs),
    /// C-MOVE SCU: ask a peer to push a study to another AE title it knows
    Movescu(MovescuArgs),
    /// DICOM storage SCP: accept associations and receive C-STORE'd instances
    Storescp(StorescpArgs),
}

#[derive(Args)]
struct ScuCommonArgs {
    /// Peer address as HOST:PORT
    destination: String,

    /// Our AE title (Calling AE)
    #[arg(long = "calling-aet", short = 'a', default_value = DEFAULT_AE_TITLE)]
    calling_ae: String,

    /// Peer's AE title (Called AE), if it requires one to match
    #[arg(long = "called-aet", short = 'c')]
    called_ae: Option<String>,

    /// Absolute timeout in seconds for the whole operation (connect through release)
    #[arg(long)]
    timeout: Option<u64>,

    /// Log association negotiation, presentation contexts, and each DIMSE command/response to stderr
    #[arg(long, short = 'v', action = ArgAction::SetTrue)]
    verbose: bool,
}

#[derive(Args)]
struct EchoscuArgs {
    #[command(flatten)]
    common: ScuCommonArgs,
}

#[derive(Args)]
struct StorescuArgs {
    #[command(flatten)]
    common: ScuCommonArgs,

    /// DICOM file(s) or director(ies) to send (directories are scanned recursively)
    #[arg(required = true, num_args = 1..)]
    files: Vec<PathBuf>,

    /// Our advertised max PDU length
    #[arg(long = "max-pdu", default_value_t = DEFAULT_SCU_MAX_PDU)]
    max_pdu: u32,

    /// Propose only each file's native transfer syntax; a peer that can't accept it fails that
    /// file rather than receiving a transcoded copy
    #[arg(long, action = ArgAction::SetTrue)]
    never_transcode: bool,
}

#[derive(Args)]
struct FindscuArgs {
    #[command(flatten)]
    common: ScuCommonArgs,

    /// Query key as KEY=VALUE (match) or bare KEY (return key, universal match). Repeatable.
    #[arg(short = 'k', long = "key", action = ArgAction::Append, value_name = "KEY[=VALUE]")]
    keys: Vec<String>,

    /// Our advertised max PDU length
    #[arg(long = "max-pdu", default_value_t = DEFAULT_SCU_MAX_PDU)]
    max_pdu: u32,
}

#[derive(Args)]
struct MovescuArgs {
    #[command(flatten)]
    common: ScuCommonArgs,

    /// AE title to move the study to (must already be known to the peer)
    move_destination_ae: String,

    /// StudyInstanceUID to retrieve
    study_instance_uid: String,

    /// Our advertised max PDU length
    #[arg(long = "max-pdu", default_value_t = DEFAULT_SCU_MAX_PDU)]
    max_pdu: u32,
}

#[derive(Args)]
struct StorescpArgs {
    /// Port to listen on (0 picks an ephemeral port)
    port: u16,

    /// Our AE title
    #[arg(long = "ae-title", short = 'a', default_value = DEFAULT_AE_TITLE)]
    ae_title: String,

    /// Directory to write received instances under (as S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm)
    #[arg(long = "cache-path", default_value = ".")]
    cache_path: PathBuf,

    /// Our advertised max PDU length
    #[arg(long = "max-pdu", default_value_t = DEFAULT_SCP_MAX_PDU)]
    max_pdu: u32,

    /// How long (seconds) an established association may sit idle before being dropped
    #[arg(long = "idle-timeout", default_value_t = DEFAULT_SCP_IDLE_TIMEOUT_SECS)]
    idle_timeout: u64,

    /// Log association negotiation, presentation contexts, and each DIMSE command/response to stderr
    #[arg(long, short = 'v', action = ArgAction::SetTrue)]
    verbose: bool,
}

struct StderrLogger;

impl DimseLogger for StderrLogger {
    fn log(&self, message: String) {
        eprintln!("[dcmtalk] {message}");
    }
}

fn logger_box(verbose: bool) -> Option<Box<dyn DimseLogger>> {
    if verbose {
        Some(Box::new(StderrLogger))
    } else {
        None
    }
}

fn logger_arc(verbose: bool) -> Option<Arc<dyn DimseLogger>> {
    if verbose {
        Some(Arc::new(StderrLogger))
    } else {
        None
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli.command) {
        eprintln!("dcmtalk: {error}");
        std::process::exit(1);
    }
}

fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Echoscu(args) => run_echoscu(args),
        Command::Storescu(args) => run_storescu(args),
        Command::Findscu(args) => run_findscu(args),
        Command::Movescu(args) => run_movescu(args),
        Command::Storescp(args) => run_storescp(args),
    }
}

/// PS3.7 Annex C status codes worth naming for a human reading CLI output; anything else is just
/// shown as hex. Not exhaustive - every peer's specific failure statuses aren't worth enumerating
/// here when the hex code plus the peer's own log is what actually diagnoses it.
fn status_description(status: u16) -> &'static str {
    match status {
        0x0000 => "Success",
        0xFF00 | 0xFF01 => "Pending",
        0xFE00 => "Cancel",
        0xA701 | 0xA702 => "Refused: Out of Resources",
        0xA801 => "Move Destination Unknown",
        0xA900 => "Identifier Does Not Match SOP Class",
        0xC000..=0xCFFF => "Unable to Process",
        0xB000..=0xBFFF => "Warning",
        _ => "Failure",
    }
}

fn other_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    io::Error::new(ErrorKind::Other, message.into()).into()
}

fn run_echoscu(args: EchoscuArgs) -> Result<(), Box<dyn std::error::Error>> {
    let common = args.common;
    let options = EchoScuOptions {
        calling_ae_title: common.calling_ae,
        called_ae_title: common.called_ae,
        timeout: common.timeout.map(Duration::from_secs),
        on_log: logger_box(common.verbose),
        cancel: None,
    };

    let status = echo_scu(&common.destination, options)?;
    println!("C-ECHO status=0x{status:04X} ({})", status_description(status));
    if status != 0x0000 {
        return Err(other_error(format!("C-ECHO failed with status 0x{status:04X}")));
    }
    Ok(())
}

fn collect_dicom_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        collect_dicom_files_from(path, &mut files);
    }
    files
}

fn collect_dicom_files_from(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            collect_dicom_files_from(&entry.path(), files);
        }
    } else if path.is_file() {
        files.push(path.to_owned());
    }
}

fn run_storescu(args: StorescuArgs) -> Result<(), Box<dyn std::error::Error>> {
    let common = args.common;
    let files = collect_dicom_files(&args.files);
    if files.is_empty() {
        return Err(other_error("no files found under the given path(s)"));
    }

    let options = StoreScuOptions {
        calling_ae_title: common.calling_ae,
        called_ae_title: common.called_ae,
        max_pdu_length: args.max_pdu,
        never_transcode: args.never_transcode,
        timeout: common.timeout.map(Duration::from_secs),
        on_log: logger_box(common.verbose),
        cancel: None,
    };

    let results: Vec<StoreScuResult> = store_scu(&common.destination, &files, options)?;

    let mut failures = 0usize;
    for result in &results {
        if result.status != 0x0000 {
            failures += 1;
        }
        println!(
            "{} status=0x{:04X} ({})",
            result.sop_instance_uid,
            result.status,
            status_description(result.status)
        );
    }
    println!("{} instance(s) sent, {} failed", results.len(), failures);

    if failures > 0 {
        return Err(other_error(format!("{failures} of {} instance(s) failed", results.len())));
    }
    Ok(())
}

fn parse_query_keys(keys: &[String]) -> HashMap<String, String> {
    let mut query = HashMap::new();
    for raw in keys {
        match raw.split_once('=') {
            Some((key, value)) => {
                query.insert(key.trim().to_owned(), value.to_owned());
            }
            None => {
                query.insert(raw.trim().to_owned(), String::new());
            }
        }
    }
    query
}

fn run_findscu(args: FindscuArgs) -> Result<(), Box<dyn std::error::Error>> {
    let common = args.common;
    let query = parse_query_keys(&args.keys);

    let options = FindScuOptions {
        calling_ae_title: common.calling_ae,
        called_ae_title: common.called_ae,
        max_pdu_length: args.max_pdu,
        timeout: common.timeout.map(Duration::from_secs),
        on_log: logger_box(common.verbose),
        cancel: None,
    };

    let matches = find_scu(&common.destination, &query, options)?;
    for json in &matches {
        println!("{json}");
    }
    eprintln!("{} match(es)", matches.len());
    Ok(())
}

fn run_movescu(args: MovescuArgs) -> Result<(), Box<dyn std::error::Error>> {
    let common = args.common;
    let options = MoveScuOptions {
        calling_ae_title: common.calling_ae,
        called_ae_title: common.called_ae,
        max_pdu_length: args.max_pdu,
        timeout: common.timeout.map(Duration::from_secs),
        stale_data_path: None,
        stale_data_timeout: None,
        on_log: logger_box(common.verbose),
        cancel: None,
    };

    let result: MoveScuResult =
        move_scu(&common.destination, &args.move_destination_ae, &args.study_instance_uid, options)?;

    println!(
        "C-MOVE status=0x{:04X} ({}) completed={} failed={} warning={} remaining={}{}",
        result.status,
        status_description(result.status),
        result.completed,
        result.failed,
        result.warning,
        result.remaining,
        if result.cancelled { " (cancelled)" } else { "" }
    );

    if !result.cancelled && result.status != 0x0000 {
        return Err(other_error(format!("C-MOVE failed with status 0x{:04X}", result.status)));
    }
    Ok(())
}

/// Minimal `ScpHandlers`: C-STORE is handled entirely by the library (this is what a standalone
/// receiver needs); C-FIND/C-MOVE have no backing query index or retrieve queue to serve from
/// here, so they're answered "unable to process" rather than pretending to support them.
struct StandaloneScpHandlers;

impl ScpHandlers for StandaloneScpHandlers {
    fn on_find(&self, _filter: &HashMap<String, String>) -> Result<Vec<HashMap<String, String>>, String> {
        Err("C-FIND is not supported by dcmtalk storescp".to_owned())
    }

    fn on_move(&self, _study_instance_uid: &str, _move_destination_ae: &str) -> Result<bool, String> {
        Err("C-MOVE is not supported by dcmtalk storescp".to_owned())
    }

    fn on_association_complete(&self, stored_instances_by_study: &HashMap<String, Vec<String>>) {
        for (study_instance_uid, paths) in stored_instances_by_study {
            println!("stored study {study_instance_uid}: {} instance(s)", paths.len());
            for path in paths {
                println!("  {path}");
            }
        }
    }
}

fn run_storescp(args: StorescpArgs) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&args.cache_path)?;

    let options = ScpOptions {
        ae_title: args.ae_title.clone(),
        max_pdu_length: args.max_pdu,
        idle_timeout: Duration::from_secs(args.idle_timeout),
        on_log: logger_arc(args.verbose),
    };

    let scp = start_scp(args.port, args.cache_path.clone(), Arc::new(StandaloneScpHandlers), options)?;

    println!(
        "dcmtalk storescp listening on port {} (AE title {:?}, cache {}). Press Ctrl+C to stop.",
        scp.local_port(),
        args.ae_title,
        args.cache_path.display()
    );

    loop {
        std::thread::park();
    }
}
