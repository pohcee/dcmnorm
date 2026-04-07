use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use clap::{ArgAction, Parser, ValueEnum};
use dcmnorm::dicom_io::{
    jpeg2000_backend_name, list_transfer_syntax_support, read_dicom_bytes, read_dicom_json_with_options,
    transcode_dicom_object, write_dicom_file, write_dicom_json_with_options,
    DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    DicomJsonWriteOptions,
};

#[derive(Parser, Debug)]
#[command(name = "dcmnorm")]
#[command(about = "Convert between DICOM and JSON")]
#[command(long_about = "Convert between DICOM and flattened or standard DICOM JSON. The CLI infers whether to run DICOM-to-JSON or JSON-to-DICOM from the input and output file types.")]
struct Cli {
    #[arg(value_name = "INPUT")]
    input: Option<PathBuf>,

    #[arg(value_name = "OUTPUT")]
    output: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = JsonFormat::Flat)]
    format: JsonFormat,

    #[arg(long, value_enum, default_value_t = KeyFormat::Name)]
    keys: KeyFormat,

    #[arg(
        long,
        value_enum,
        default_value_t = BulkDataMode::Uri,
        help = "Bulk data encoding mode for DICOM to JSON. In uri mode, values of 32 bytes or less still use InlineBinary automatically"
    )]
    bulk_data: BulkDataMode,

    #[arg(long, value_name = "SOURCE")]
    bulk_data_source: Option<PathBuf>,

    #[arg(
        long,
        value_name = "UID",
        help = "Target transfer syntax UID for DICOM-to-DICOM transcoding"
    )]
    transfer_syntax: Option<String>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "List transfer syntaxes known to this build and exit"
    )]
    list_transfer_syntaxes: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum JsonFormat {
    Flat,
    Standard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum KeyFormat {
    Name,
    Hex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum BulkDataMode {
    Inline,
    Uri,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileKind {
    Json,
    Dicom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    DicomToJson,
    DicomToDicom,
    JsonToDicom,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if cli.list_transfer_syntaxes {
        print_transfer_syntax_support()?;
        return Ok(());
    }

    let input_path = cli.input.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "an input path is required unless --list-transfer-syntaxes is set",
        )
    })?;

    let input_bytes = fs::read(input_path)?;
    let direction = infer_direction(&cli, &input_bytes)?;

    match direction {
        Direction::DicomToJson => run_dicom_to_json(&cli, &input_bytes),
        Direction::DicomToDicom => run_dicom_to_dicom(&cli, &input_bytes),
        Direction::JsonToDicom => run_json_to_dicom(&cli, &input_bytes),
    }
}

fn print_transfer_syntax_support() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();
    let support = list_transfer_syntax_support();
    let uid_width = support
        .iter()
        .map(|entry| entry.uid.len())
        .max()
        .unwrap_or(3)
        .max("UID".len());
    let bool_width = "DATASET_WRITE".len();
    let engine_width = support
        .iter()
        .map(|entry| transfer_syntax_engine(entry).len())
        .max()
        .unwrap_or(6)
        .max("ENGINE".len());

    writeln!(
        stdout,
        "{:<uid_width$}  {:<bool_width$}  {:<bool_width$}  {:<bool_width$}  {:<bool_width$}  {:<engine_width$}  {}",
        "UID",
        "DATASET_READ",
        "DATASET_WRITE",
        "PIXEL_DECODE",
        "PIXEL_ENCODE",
        "ENGINE",
        "NAME",
        uid_width = uid_width,
        bool_width = bool_width,
        engine_width = engine_width,
    )?;

    for entry in support {
        writeln!(
            stdout,
            "{:<uid_width$}  {:<bool_width$}  {:<bool_width$}  {:<bool_width$}  {:<bool_width$}  {:<engine_width$}  {}",
            entry.uid,
            yes_no(entry.can_read_dataset),
            yes_no(entry.can_write_dataset),
            yes_no(entry.can_decode_pixel_data),
            yes_no(entry.can_encode_pixel_data),
            transfer_syntax_engine(&entry),
            entry.name,
            uid_width = uid_width,
            bool_width = bool_width,
            engine_width = engine_width,
        )?;
    }

    Ok(())
}

fn transfer_syntax_engine(entry: &dcmnorm::dicom_io::TransferSyntaxSupport) -> &'static str {
    if entry.name.to_ascii_lowercase().contains("uncompressed") {
        return "n/a";
    }

    if is_jpeg2000_transfer_syntax_uid(&entry.uid) {
        return jpeg2000_backend_name();
    }

    if !entry.encapsulated_pixel_data {
        return "n/a";
    }

    if entry.can_decode_pixel_data || entry.can_encode_pixel_data {
        return "builtin";
    }

    "n/a"
}

fn is_jpeg2000_transfer_syntax_uid(uid: &str) -> bool {
    matches!(
        uid,
        "1.2.840.10008.1.2.4.90"
            | "1.2.840.10008.1.2.4.91"
            | "1.2.840.10008.1.2.4.92"
            | "1.2.840.10008.1.2.4.93"
    )
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn run_dicom_to_json(cli: &Cli, input_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if cli.bulk_data_source.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--bulk-data-source is only valid when converting JSON to DICOM",
        )
        .into());
    }

    if cli.transfer_syntax.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--transfer-syntax is only valid when converting DICOM to DICOM",
        )
        .into());
    }

    let object = read_dicom_bytes(input_bytes)?;
    let bulk_data_mode = match cli.bulk_data {
        BulkDataMode::Inline => DicomJsonBulkDataMode::InlineBinary,
        BulkDataMode::Uri => DicomJsonBulkDataMode::Uri,
    };

    let output = write_dicom_json_with_options(
        &object,
        DicomJsonWriteOptions {
            format: match cli.format {
                JsonFormat::Flat => DicomJsonFormat::Flat,
                JsonFormat::Standard => DicomJsonFormat::Standard,
            },
            bulk_data_mode,
            key_style: match cli.keys {
                KeyFormat::Name => DicomJsonKeyStyle::Name,
                KeyFormat::Hex => DicomJsonKeyStyle::Hex,
            },
            bulk_data_source: if bulk_data_mode == DicomJsonBulkDataMode::Uri {
                Some(input_bytes)
            } else {
                None
            },
        },
    )?;

    if let Some(path) = &cli.output {
        fs::write(path, output)?;
    } else {
        let mut stdout = io::stdout().lock();
        stdout.write_all(output.as_bytes())?;
        stdout.write_all(b"\n")?;
    }

    Ok(())
}

fn run_dicom_to_dicom(cli: &Cli, input_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let output_path = cli.output.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "DICOM to DICOM transcoding requires an output path",
        )
    })?;
    let target_transfer_syntax = cli.transfer_syntax.as_deref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "DICOM to DICOM transcoding requires --transfer-syntax <UID>",
        )
    })?;

    if cli.keys != KeyFormat::Name {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--keys is only valid when converting DICOM to JSON",
        )
        .into());
    }

    if cli.bulk_data != BulkDataMode::Uri {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--bulk-data is only valid when converting DICOM to JSON",
        )
        .into());
    }

    if cli.bulk_data_source.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--bulk-data-source is only valid when converting JSON to DICOM",
        )
        .into());
    }

    if cli.format != JsonFormat::Flat {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--format is only valid for DICOM to JSON and JSON to DICOM",
        )
        .into());
    }

    let object = read_dicom_bytes(input_bytes)?;
    let transcoded = transcode_dicom_object(&object, target_transfer_syntax)?;
    write_dicom_file(&transcoded, output_path)?;
    Ok(())
}

fn run_json_to_dicom(cli: &Cli, input_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let output_path = cli.output.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "JSON to DICOM conversion requires an output path",
        )
    })?;

    if cli.keys != KeyFormat::Name {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--keys is only valid when converting DICOM to JSON",
        )
        .into());
    }

    if cli.bulk_data != BulkDataMode::Uri {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--bulk-data is only valid when converting DICOM to JSON",
        )
        .into());
    }

    if cli.transfer_syntax.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--transfer-syntax is only valid when converting DICOM to DICOM",
        )
        .into());
    }

    let json = std::str::from_utf8(input_bytes)?;
    let bulk_data_source = cli
        .bulk_data_source
        .as_ref()
        .map(fs::read)
        .transpose()?;

    let object = read_dicom_json_with_options(
        json,
        DicomJsonReadOptions {
            format: match cli.format {
                JsonFormat::Flat => DicomJsonFormat::Flat,
                JsonFormat::Standard => DicomJsonFormat::Standard,
            },
            bulk_data_source: bulk_data_source.as_deref(),
        },
    )?;

    write_dicom_file(&object, output_path)?;
    Ok(())
}

fn infer_direction(cli: &Cli, input_bytes: &[u8]) -> Result<Direction, Box<dyn std::error::Error>> {
    let input = cli.input.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "an input path is required unless --list-transfer-syntaxes is set",
        )
    })?;
    let input_kind = detect_input_kind(input, input_bytes)?;

    match (&cli.output, input_kind) {
        (Some(output), FileKind::Dicom) => match detect_output_kind(output) {
            Some(FileKind::Json) => Ok(Direction::DicomToJson),
            Some(FileKind::Dicom) => {
                if cli.transfer_syntax.is_some() {
                    Ok(Direction::DicomToDicom)
                } else {
                    Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        "DICOM input with DICOM output requires --transfer-syntax <UID>",
                    )
                    .into())
                }
            }
            None => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "could not determine output type; use a .json, .dcm, or .dicom extension",
            )
            .into()),
        },
        (Some(output), FileKind::Json) => match detect_output_kind(output) {
            Some(FileKind::Dicom) => Ok(Direction::JsonToDicom),
            Some(FileKind::Json) => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "JSON input with JSON output is not a supported conversion",
            )
            .into()),
            None => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "could not determine output type; use a .json, .dcm, or .dicom extension",
            )
            .into()),
        },
        (None, FileKind::Dicom) => Ok(Direction::DicomToJson),
        (None, FileKind::Json) => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "JSON to DICOM conversion requires an output path",
        )
        .into()),
    }
}

fn detect_input_kind(
    path: &Path,
    input_bytes: &[u8],
) -> Result<FileKind, Box<dyn std::error::Error>> {
    if let Some(kind) = detect_kind_from_extension(path) {
        return Ok(kind);
    }

    if looks_like_json(input_bytes) {
        return Ok(FileKind::Json);
    }

    if looks_like_dicom(input_bytes) || read_dicom_bytes(input_bytes).is_ok() {
        return Ok(FileKind::Dicom);
    }

    Err(io::Error::new(
        ErrorKind::InvalidInput,
        "could not determine input type; use a .json, .dcm, or .dicom extension",
    )
    .into())
}

fn detect_output_kind(path: &Path) -> Option<FileKind> {
    detect_kind_from_extension(path)
}

fn detect_kind_from_extension(path: &Path) -> Option<FileKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();

    match extension.as_str() {
        "json" => Some(FileKind::Json),
        "dcm" | "dicom" => Some(FileKind::Dicom),
        _ => None,
    }
}

fn looks_like_json(input_bytes: &[u8]) -> bool {
    let trimmed = input_bytes
        .iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace)
        .collect::<Vec<_>>();

    matches!(trimmed.first(), Some(b'{') | Some(b'['))
}

fn looks_like_dicom(input_bytes: &[u8]) -> bool {
    input_bytes.len() >= 132 && &input_bytes[128..132] == b"DICM"
}