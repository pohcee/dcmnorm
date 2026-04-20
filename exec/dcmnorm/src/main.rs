use std::fs;
use std::io::{self, BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, ValueEnum};
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::{Tag, VR};
use dicom_dictionary_std::tags;
use dicom_dictionary_std::StandardDataDictionary;
use dcmnorm::dicom_io::{
    jpeg2000_backend_name, list_transfer_syntax_support, read_dicom_bytes,
    read_dicom_json_with_options, render_all_dicom_frames, render_dicom_frame,
    transcode_dicom_object, write_dicom_file, write_dicom_json_with_options,
    DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    DicomJsonWriteOptions, RenderOutputFormat, RenderPipelineOptions,
};
use sha2::{Digest, Sha256};

#[derive(Parser, Debug)]
#[command(name = "dcmnorm")]
#[command(version)]
#[command(about = "Convert, transcode, and render DICOM data")]
#[command(long_about = "Convert between DICOM and flattened or standard DICOM JSON, transcode DICOM transfer syntaxes, render DICOM frames to raw/PNG/JPEG/MPEG4 outputs, and list transfer-syntax support for the current build. The CLI infers the operation from the input and output file types unless an explicit mode flag is provided.")]
#[command(arg_required_else_help = true)]
struct Cli {
    #[arg(value_name = "INPUT", help_heading = "General", display_order = 1)]
    input: Option<PathBuf>,

    #[arg(value_name = "OUTPUT", help_heading = "General", display_order = 2)]
    output: Option<PathBuf>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "List transfer syntaxes known to this build and exit",
        help_heading = "General",
        display_order = 3
    )]
    list_transfer_syntaxes: bool,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Emit verbose conversion and rendering diagnostics",
        help_heading = "General",
        display_order = 4
    )]
    verbose: bool,

    #[arg(
        short = 'I',
        long = "stdin-paths",
        action = ArgAction::SetTrue,
        help = "Read input paths from stdin, one per line (e.g. find . -name '*.dcm' | dcmnorm -I)",
        help_heading = "General",
        display_order = 5
    )]
    stdin_paths: bool,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Overwrite each input file in place. With DICOM input this writes updated DICOM back to the same path",
        help_heading = "General",
        display_order = 6
    )]
    overwrite: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = JsonFormat::Flat,
        help_heading = "JSON Conversion",
        display_order = 10
    )]
    format: JsonFormat,

    #[arg(
        long,
        value_enum,
        default_value_t = KeyFormat::Name,
        help_heading = "JSON Conversion",
        display_order = 11
    )]
    keys: KeyFormat,

    #[arg(
        long,
        value_enum,
        default_value_t = BulkDataMode::Uri,
        help = "Bulk data encoding mode for DICOM to JSON. In uri mode, values over 32 bytes use BulkDataURI (relative by default; use --bulk-data-source with no value to embed file:// URIs)",
        help_heading = "JSON Conversion",
        display_order = 12
    )]
    bulk_data: BulkDataMode,

    #[arg(
        long,
        value_name = "SOURCE",
        num_args = 0..=1,
        default_missing_value = "",
        help = "For JSON-to-DICOM: path to the original DICOM file used to resolve BulkDataURIs. For DICOM-to-JSON with --bulk-data uri: pass this flag with no value to embed the input file path as file:// in each BulkDataURI",
        help_heading = "JSON Conversion",
        display_order = 13
    )]
    bulk_data_source: Option<String>,

    #[arg(
        long,
        value_name = "UID",
        help = "Target transfer syntax UID for DICOM-to-DICOM transcoding",
        help_heading = "DICOM Transcoding",
        display_order = 20
    )]
    transfer_syntax: Option<String>,

    #[arg(
        long,
        value_name = "KEY=VALUE",
        action = ArgAction::Append,
        help = "Set or replace a DICOM element value. KEY can be a keyword (e.g. SOPClassUID) or tag expression (e.g. (0008,0016)). Repeat this option to set multiple elements",
        help_heading = "DICOM Editing",
        display_order = 21
    )]
    set: Vec<String>,

    #[arg(
        long,
        value_enum,
        help = "Render DICOM input to this format (raw/png/jpeg/mpeg4). For MPEG4 files, use the .mp4 output extension; if omitted, the format is inferred from the output extension",
        help_heading = "Rendering",
        display_order = 30
    )]
    render_format: Option<RenderFormat>,

    #[arg(
        long,
        default_value_t = 0,
        help = "Zero-based frame index to render",
        help_heading = "Rendering",
        display_order = 31
    )]
    render_frame: usize,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Render and export all frames for multiframe images. For image outputs, OUTPUT is expanded to STEM_000001.EXT, STEM_000002.EXT, and so on",
        help_heading = "Rendering",
        display_order = 32
    )]
    render_all_frames: bool,

    #[arg(
        long,
        value_name = "FPS",
        help = "Frames per second when writing MPEG4/.mp4 output (defaults to DICOM frame rate metadata when available, else 24)",
        help_heading = "Rendering",
        display_order = 33
    )]
    render_fps: Option<f64>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Disable modality LUT during rendering",
        help_heading = "Rendering",
        display_order = 34
    )]
    no_modality_lut: bool,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Disable VOI LUT / windowing during rendering",
        help_heading = "Rendering",
        display_order = 35
    )]
    no_voi_lut: bool,

    #[arg(
        long,
        value_name = "FLOAT",
        help = "Override VOI window center for rendering",
        help_heading = "Rendering",
        display_order = 36
    )]
    window_center: Option<f64>,

    #[arg(
        long,
        value_name = "FLOAT",
        help = "Override VOI window width for rendering",
        help_heading = "Rendering",
        display_order = 37
    )]
    window_width: Option<f64>,

    #[arg(
        long,
        default_value_t = 90,
        help = "JPEG quality for rendered JPEG output (1-100)",
        help_heading = "Rendering",
        display_order = 38
    )]
    jpeg_quality: u8,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Set the output width in pixels. If --output-height is also set, the image is scaled exactly; otherwise the height is computed from the aspect ratio",
        help_heading = "Rendering",
        display_order = 39
    )]
    output_width: Option<u32>,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Set the output height in pixels. If --output-width is also set, the image is scaled exactly; otherwise the width is computed from the aspect ratio",
        help_heading = "Rendering",
        display_order = 40
    )]
    output_height: Option<u32>,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Scale output while preserving aspect ratio so the longer side equals this value",
        help_heading = "Rendering",
        display_order = 41
    )]
    scale_max_size: Option<u32>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RenderFormat {
    Raw,
    Png,
    Jpeg,
    Mpeg4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileKind {
    Json,
    Dicom,
    Render,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    DicomToJson,
    DicomToDicom,
    DicomToRender,
    JsonToDicom,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let version_with_hash = cli_version_with_binary_hash();
    let version_static: &'static str = Box::leak(version_with_hash.into_boxed_str());
    let matches = Cli::command().version(version_static).get_matches();
    let cli = Cli::from_arg_matches(&matches).expect("clap generated invalid matches");

    if cli.list_transfer_syntaxes {
        print_transfer_syntax_support()?;
        return Ok(());
    }

    if cli.stdin_paths {
        let stdin = io::stdin();
        let mut any_error = false;
        for line in stdin.lock().lines() {
            let line = line?;
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let input_path = PathBuf::from(&line);
            if let Err(e) = process_one(&cli, &input_path) {
                eprintln!("{}: {e}", input_path.display());
                any_error = true;
            }
        }
        if any_error {
            return Err(io::Error::new(ErrorKind::Other, "one or more inputs failed").into());
        }
        return Ok(());
    }

    let input_path = cli.input.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "an input path is required unless --list-transfer-syntaxes is set",
        )
    })?;

    process_one(&cli, input_path)
}

fn process_one(cli: &Cli, input_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let input_bytes = fs::read(input_path)?;
    let direction = infer_direction(cli, input_path, &input_bytes)?;

    match direction {
        Direction::DicomToJson => run_dicom_to_json(cli, input_path, &input_bytes),
        Direction::DicomToDicom => run_dicom_to_dicom(cli, input_path, &input_bytes),
        Direction::DicomToRender => run_dicom_to_render(cli, &input_bytes),
        Direction::JsonToDicom => run_json_to_dicom(cli, &input_bytes),
    }
}

fn cli_version_with_binary_hash() -> String {
    let base_version = env!("CARGO_PKG_VERSION");
    match running_binary_sha256_prefix(12) {
        Some(hash_prefix) => format!("{base_version}-{hash_prefix}"),
        None => base_version.to_string(),
    }
}

fn running_binary_sha256_prefix(prefix_len: usize) -> Option<String> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_bytes = fs::read(exe_path).ok()?;

    let mut hasher = Sha256::new();
    hasher.update(&exe_bytes);
    let digest = hasher.finalize();

    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").ok()?;
    }

    Some(hex.chars().take(prefix_len).collect())
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

fn path_to_file_uri(path: &Path) -> Option<String> {
    let abs = path.canonicalize().ok()?;
    let s = abs.to_str()?;
    // Encode spaces and percent signs; other characters used in typical paths are safe.
    let encoded: String = s
        .chars()
        .flat_map(|c| match c {
            ' ' => vec!['%', '2', '0'],
            '%' => vec!['%', '2', '5'],
            c => vec![c],
        })
        .collect();
    Some(format!("file://{encoded}"))
}

fn run_dicom_to_json(
    cli: &Cli,
    input_path: &Path,
    input_bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    validate_no_render_flags(cli)?;

    if matches!(&cli.bulk_data_source, Some(s) if !s.is_empty()) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--bulk-data-source with a path is only valid when converting JSON to DICOM; use --bulk-data-source without a value to embed the input file:// URI in BulkDataURIs",
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

    let mut object = read_dicom_bytes(input_bytes)?;
    apply_attribute_overrides(cli, &mut object)?;
    verbose_log(
        cli,
        format!(
            "Converting DICOM to JSON (format={:?}, keys={:?}, bulk_data={:?})",
            cli.format, cli.keys, cli.bulk_data
        ),
    );
    let bulk_data_mode = match cli.bulk_data {
        BulkDataMode::Inline => DicomJsonBulkDataMode::InlineBinary,
        BulkDataMode::Uri => DicomJsonBulkDataMode::Uri,
    };

    // Embed the input file:// URI in BulkDataURIs only when the user explicitly
    // passes --bulk-data-source without a value.
    let uri_base_owned: Option<String> =
        if bulk_data_mode == DicomJsonBulkDataMode::Uri
            && cli.bulk_data_source.as_deref() == Some("")
        {
            path_to_file_uri(input_path)
        } else {
            None
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
            bulk_data_uri_base: uri_base_owned.as_deref(),
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

fn run_dicom_to_dicom(
    cli: &Cli,
    input_path: &Path,
    input_bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    validate_no_render_flags(cli)?;

    if cli.overwrite && cli.output.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--overwrite cannot be combined with an explicit output path",
        )
        .into());
    }

    let output_path = if cli.overwrite {
        input_path
    } else {
        cli.output.as_deref().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "DICOM to DICOM output requires either an output path or --overwrite",
            )
        })?
    };

    if cli.transfer_syntax.is_none() && cli.set.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "DICOM to DICOM output requires --transfer-syntax <UID> and/or at least one --set KEY=VALUE",
        )
        .into());
    }

    let target_transfer_syntax = cli.transfer_syntax.as_deref();

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

    let mut object = read_dicom_bytes(input_bytes)?;
    apply_attribute_overrides(cli, &mut object)?;
    if let Some(target_transfer_syntax) = target_transfer_syntax {
        verbose_log(
            cli,
            format!(
                "Transcoding DICOM to transfer syntax {} -> {}",
                object.meta().transfer_syntax(),
                target_transfer_syntax
            ),
        );
        let transcoded = transcode_dicom_object(&object, target_transfer_syntax)?;
        write_dicom_file(&transcoded, output_path)?;
    } else {
        verbose_log(
            cli,
            format!("Writing updated DICOM to {}", output_path.display()),
        );
        write_dicom_file(&object, output_path)?;
    }

    Ok(())
}

fn run_dicom_to_render(cli: &Cli, input_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
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

    if cli.transfer_syntax.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--transfer-syntax is only valid when converting DICOM to DICOM",
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

    let output_path = cli.output.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "DICOM rendering requires an output path",
        )
    })?;

    let mut object = read_dicom_bytes(input_bytes)?;
    apply_attribute_overrides(cli, &mut object)?;
    let format = resolve_render_format(cli, output_path)?;
    verbose_log(
        cli,
        format!(
            "Rendering DICOM to {:?} (output={}, frame={}, all_frames={}, modality_lut={}, voi_lut={}, jpeg_quality={})",
            format,
            output_path.display(),
            cli.render_frame,
            cli.render_all_frames || format == RenderFormat::Mpeg4,
            !cli.no_modality_lut,
            !cli.no_voi_lut,
            cli.jpeg_quality
        ),
    );
    let options = RenderPipelineOptions {
        frame_index: cli.render_frame,
        apply_modality_lut: !cli.no_modality_lut,
        apply_voi_lut: !cli.no_voi_lut,
        window_center: cli.window_center,
        window_width: cli.window_width,
        jpeg_quality: cli.jpeg_quality,
        output_width: cli.output_width,
        output_height: cli.output_height,
        scale_max_size: cli.scale_max_size,
    };

    if format == RenderFormat::Mpeg4 {
        if cli.render_frame != 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "MPEG4 rendering always uses all frames; --render-frame must be 0",
            )
            .into());
        }

        let fps = cli
            .render_fps
            .or_else(|| default_render_fps_from_dicom(&object))
            .unwrap_or(24.0);

        verbose_log(cli, format!("Using MPEG4 frame rate: {fps}"));
        write_mpeg4(&object, output_path, &options, fps, cli.verbose)?;
        return Ok(());
    }

    if cli.render_fps.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--render-fps is only valid for MPEG4 output",
        )
        .into());
    }

    let has_scale = cli.scale_max_size.is_some();
    let has_output = cli.output_width.is_some() || cli.output_height.is_some();
    if has_scale && has_output {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--scale-max-size cannot be combined with --output-width/--output-height",
        )
        .into());
    }

    if cli.render_all_frames {
        let rendered = render_all_dicom_frames(&object, to_render_output_format(format), &options)?;
        verbose_log(cli, format!("Rendered {} frame(s)", rendered.len()));
        write_multi_frame_outputs(output_path, format, rendered)?;
        return Ok(());
    }

    let rendered = render_dicom_frame(&object, to_render_output_format(format), &options)?;
    verbose_log(
        cli,
        format!(
            "Rendered frame to {}x{} {}-sample output",
            rendered.width, rendered.height, rendered.samples_per_pixel
        ),
    );
    fs::write(output_path, rendered.bytes)?;
    Ok(())
}

fn run_json_to_dicom(cli: &Cli, input_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    validate_no_render_flags(cli)?;

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
    if cli.bulk_data_source.as_deref() == Some("") {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--bulk-data-source requires a path when converting JSON to DICOM",
        )
        .into());
    }

    let bulk_data_source = cli
        .bulk_data_source
        .as_deref()
        .map(fs::read)
        .transpose()?;

    let mut object = read_dicom_json_with_options(
        json,
        DicomJsonReadOptions {
            format: match cli.format {
                JsonFormat::Flat => DicomJsonFormat::Flat,
                JsonFormat::Standard => DicomJsonFormat::Standard,
            },
            bulk_data_source: bulk_data_source.as_deref(),
        },
    )?;
    apply_attribute_overrides(cli, &mut object)?;

    verbose_log(
        cli,
        format!(
            "Converting JSON to DICOM (format={:?}, output={})",
            cli.format,
            output_path.display()
        ),
    );
    write_dicom_file(&object, output_path)?;
    Ok(())
}

fn apply_attribute_overrides(
    cli: &Cli,
    object: &mut dicom_object::DefaultDicomObject,
) -> Result<(), Box<dyn std::error::Error>> {
    for assignment in &cli.set {
        let (tag, vr, value) = parse_attribute_override(assignment)?;
        object.put_str(tag, vr, value);
        verbose_log(
            cli,
            format!(
                "Set {} ({:04X},{:04X}) to {}",
                keyword_for_tag(tag),
                tag.group(),
                tag.element(),
                assignment
                    .split_once('=')
                    .map(|(_, rhs)| rhs)
                    .unwrap_or_default()
            ),
        );
    }

    Ok(())
}

fn parse_attribute_override(assignment: &str) -> Result<(Tag, VR, String), io::Error> {
    let (raw_key, raw_value) = assignment.split_once('=').ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --set value '{assignment}'; expected KEY=VALUE, for example SOPClassUID=1.2.840.10008.5.1.4.1.1.2"
            ),
        )
    })?;

    let key = raw_key.trim();
    if key.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid --set value '{assignment}'; KEY cannot be empty"),
        ));
    }

    let tag = StandardDataDictionary.parse_tag(key).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --set key '{key}'; use a DICOM keyword like SOPClassUID or a tag expression like (0008,0016)"
            ),
        )
    })?;

    let vr = StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.vr().relaxed())
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "could not determine VR for --set key '{key}'; use a standard DICOM attribute"
                ),
            )
        })?;

    if vr == VR::SQ {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "--set does not currently support sequence attributes ({key}); set non-sequence elements instead"
            ),
        ));
    }

    Ok((tag, vr, raw_value.to_owned()))
}

fn keyword_for_tag(tag: Tag) -> String {
    StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.alias().to_owned())
        .unwrap_or_else(|| format!("({:04X},{:04X})", tag.group(), tag.element()))
}

fn infer_direction(cli: &Cli, input: &Path, input_bytes: &[u8]) -> Result<Direction, Box<dyn std::error::Error>> {
    let input_kind = detect_input_kind(input, input_bytes)?;

    if cli.overwrite && cli.output.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--overwrite cannot be combined with an explicit output path",
        )
        .into());
    }

    match (&cli.output, input_kind) {
        (Some(output), FileKind::Dicom) => match detect_output_kind(output) {
            Some(FileKind::Json) => Ok(Direction::DicomToJson),
            Some(FileKind::Dicom) => {
                if cli.transfer_syntax.is_some() || !cli.set.is_empty() {
                    Ok(Direction::DicomToDicom)
                } else {
                    Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        "DICOM input with DICOM output requires --transfer-syntax <UID> and/or at least one --set KEY=VALUE",
                    )
                    .into())
                }
            }
            Some(FileKind::Render) => Ok(Direction::DicomToRender),
            None => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "could not determine output type; use .json, .dcm/.dicom, or a render extension (.jpg/.jpeg/.png/.raw/.mp4)",
            )
            .into()),
        },
        (Some(output), FileKind::Json) => match detect_output_kind(output) {
            Some(FileKind::Dicom) => Ok(Direction::JsonToDicom),
            Some(FileKind::Render) => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "JSON input cannot be rendered directly; convert to DICOM first",
            )
            .into()),
            Some(FileKind::Json) => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "JSON input with JSON output is not a supported conversion",
            )
            .into()),
            None => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "could not determine output type; use .json, .dcm/.dicom, or a render extension (.jpg/.jpeg/.png/.raw/.mp4)",
            )
            .into()),
        },
        (None, FileKind::Dicom) => {
            if cli.overwrite {
                if cli.transfer_syntax.is_some() || !cli.set.is_empty() {
                    Ok(Direction::DicomToDicom)
                } else {
                    Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        "--overwrite requires --transfer-syntax <UID> and/or at least one --set KEY=VALUE",
                    )
                    .into())
                }
            } else {
                Ok(Direction::DicomToJson)
            }
        }
        (None, FileKind::Json) => {
            if cli.overwrite {
                Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "--overwrite is only valid for DICOM input",
                )
                .into())
            } else {
                Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "JSON to DICOM conversion requires an output path",
                )
                .into())
            }
        }
        (_, FileKind::Render) => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "rendered image input is not supported; input must be DICOM or JSON",
        )
        .into()),
    }
}

fn detect_input_kind(
    path: &Path,
    input_bytes: &[u8],
) -> Result<FileKind, Box<dyn std::error::Error>> {
    if let Some(kind) = detect_kind_from_extension(path) {
        if kind != FileKind::Render {
            return Ok(kind);
        }
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
        "jpg" | "jpeg" | "png" | "raw" | "mp4" | "m4v" => Some(FileKind::Render),
        _ => None,
    }
}

fn validate_no_render_flags(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.render_format.is_some()
        || cli.render_frame != 0
        || cli.no_modality_lut
        || cli.no_voi_lut
        || cli.window_center.is_some()
        || cli.window_width.is_some()
        || cli.jpeg_quality != 90
        || cli.render_all_frames
        || cli.render_fps.is_some()
        || cli.output_width.is_some()
        || cli.output_height.is_some()
        || cli.scale_max_size.is_some()
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "render options are only valid when converting DICOM to .jpg/.jpeg/.png/.raw/.mp4",
        )
        .into());
    }

    Ok(())
}

fn resolve_render_format(
    cli: &Cli,
    output_path: &Path,
) -> Result<RenderFormat, Box<dyn std::error::Error>> {
    if let Some(format) = cli.render_format {
        return Ok(format);
    }

    let extension = output_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "render output requires --render-format when output extension is missing",
            )
        })?;

    match extension.as_str() {
        "raw" => Ok(RenderFormat::Raw),
        "png" => Ok(RenderFormat::Png),
        "jpg" | "jpeg" => Ok(RenderFormat::Jpeg),
        "mp4" | "m4v" => Ok(RenderFormat::Mpeg4),
        _ => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "render output extension must be .raw, .png, .jpg/.jpeg, or .mp4",
        )
        .into()),
    }
}

fn to_render_output_format(format: RenderFormat) -> RenderOutputFormat {
    match format {
        RenderFormat::Raw => RenderOutputFormat::Raw,
        RenderFormat::Png => RenderOutputFormat::Png,
        RenderFormat::Jpeg => RenderOutputFormat::Jpeg,
        RenderFormat::Mpeg4 => RenderOutputFormat::Png,
    }
}

fn write_multi_frame_outputs(
    output_path: &Path,
    format: RenderFormat,
    frames: Vec<dcmnorm::dicom_io::RenderFrameOutput>,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        RenderFormat::Raw => {
            let mut all_bytes = Vec::new();
            for frame in frames {
                all_bytes.extend_from_slice(&frame.bytes);
            }
            fs::write(output_path, all_bytes)?;
            Ok(())
        }
        RenderFormat::Png | RenderFormat::Jpeg => {
            if frames.is_empty() {
                return Err(io::Error::new(ErrorKind::InvalidInput, "no frames rendered").into());
            }

            for (index, frame) in frames.into_iter().enumerate() {
                let path = frame_output_path(output_path, index + 1)?;
                fs::write(path, frame.bytes)?;
            }

            Ok(())
        }
        RenderFormat::Mpeg4 => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "MPEG4 output is handled separately",
        )
        .into()),
    }
}

fn frame_output_path(base: &Path, frame_number: usize) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "invalid output filename"))?;
    let extension = base
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "output extension required"))?;

    let file_name = format!("{stem}_{frame_number:06}.{extension}");
    Ok(base.with_file_name(file_name))
}

fn write_mpeg4(
    object: &dicom_object::DefaultDicomObject,
    output_path: &Path,
    options: &RenderPipelineOptions,
    fps: f64,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !fps.is_finite() || fps <= 0.0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--render-fps must be greater than zero",
        )
        .into());
    }

    let frames = render_all_dicom_frames(object, RenderOutputFormat::Png, options)?;
    if frames.is_empty() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "no frames rendered").into());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_dir = std::env::temp_dir().join(format!("dcmnorm-render-{nonce}"));
    fs::create_dir_all(&temp_dir)?;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        for (index, frame) in frames.into_iter().enumerate() {
            let path = temp_dir.join(format!("frame_{:06}.png", index + 1));
            fs::write(path, frame.bytes)?;
        }

        let input_pattern = temp_dir.join("frame_%06d.png");
        let mut command = Command::new("ffmpeg");
        command
            .arg("-y")
            .arg("-framerate")
            .arg(format!("{fps}"))
            .arg("-i")
            .arg(input_pattern)
            .arg("-c:v")
            .arg("libx264")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg(output_path);

        if !verbose {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }

        let status = command.status();

        match status {
            Ok(exit) if exit.success() => Ok(()),
            Ok(exit) => Err(io::Error::new(
                ErrorKind::Other,
                format!("ffmpeg failed with exit status {exit}"),
            )
            .into()),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(io::Error::new(
                ErrorKind::NotFound,
                "ffmpeg executable not found in PATH (required for MPEG4 output)",
            )
            .into()),
            Err(error) => Err(io::Error::new(
                ErrorKind::Other,
                format!("failed to execute ffmpeg: {error}"),
            )
            .into()),
        }
    })();

    let _ = fs::remove_dir_all(&temp_dir);
    result
}

fn verbose_log(cli: &Cli, message: impl AsRef<str>) {
    if cli.verbose {
        eprintln!("[dcmnorm] {}", message.as_ref());
    }
}

fn default_render_fps_from_dicom(object: &dicom_object::DefaultDicomObject) -> Option<f64> {
    first_numeric_tag(object, tags::RECOMMENDED_DISPLAY_FRAME_RATE_IN_FLOAT)
        .or_else(|| first_numeric_tag(object, tags::RECOMMENDED_DISPLAY_FRAME_RATE))
        .or_else(|| first_numeric_tag(object, tags::CINE_RATE))
        .or_else(|| {
            first_numeric_tag(object, tags::FRAME_TIME)
                .filter(|frame_time_ms| *frame_time_ms > 0.0)
                .map(|frame_time_ms| 1000.0 / frame_time_ms)
        })
        .or_else(|| {
            numeric_values_tag(object, tags::FRAME_TIME_VECTOR).and_then(|values| {
                let valid = values
                    .into_iter()
                    .filter(|value| value.is_finite() && *value > 0.0)
                    .collect::<Vec<_>>();

                if valid.is_empty() {
                    return None;
                }

                let mean_ms = valid.iter().sum::<f64>() / valid.len() as f64;
                Some(1000.0 / mean_ms)
            })
        })
        .filter(|fps| fps.is_finite() && *fps > 0.0)
}

fn first_numeric_tag(object: &dicom_object::DefaultDicomObject, tag: dicom_core::Tag) -> Option<f64> {
    object
        .get(tag)
        .and_then(|element| element.to_str().ok())
        .and_then(|text| {
            text.split('\\')
                .next()
                .and_then(|part| part.trim().parse::<f64>().ok())
        })
}

fn numeric_values_tag(object: &dicom_object::DefaultDicomObject, tag: dicom_core::Tag) -> Option<Vec<f64>> {
    let text = object.get(tag).and_then(|element| element.to_str().ok())?;
    let values = text
        .split('\\')
        .filter_map(|part| part.trim().parse::<f64>().ok())
        .collect::<Vec<_>>();

    if values.is_empty() {
        None
    } else {
        Some(values)
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

#[cfg(test)]
mod tests {
    use super::{
        detect_output_kind, infer_direction, parse_attribute_override, resolve_render_format, Cli,
        Direction, FileKind, RenderFormat,
    };
    use clap::{CommandFactory, FromArgMatches};
    use dicom_core::Tag;
    use std::path::PathBuf;

    fn base_cli() -> Cli {
        Cli {
            input: None,
            output: None,
            stdin_paths: false,
            overwrite: false,
            format: super::JsonFormat::Flat,
            keys: super::KeyFormat::Name,
            bulk_data: super::BulkDataMode::Uri,
            bulk_data_source: None,
            transfer_syntax: None,
            set: Vec::new(),
            render_format: None,
            render_frame: 0,
            no_modality_lut: false,
            no_voi_lut: false,
            window_center: None,
            window_width: None,
            jpeg_quality: 90,
            render_all_frames: false,
            render_fps: None,
            output_width: None,
            output_height: None,
            scale_max_size: None,
            list_transfer_syntaxes: false,
            verbose: false,
        }
    }

    #[test]
    fn detects_mp4_output_as_render() {
        assert_eq!(
            detect_output_kind(&PathBuf::from("out.mp4")),
            Some(FileKind::Render)
        );
    }

    #[test]
    fn infers_mpeg4_from_mp4_extension() {
        let cli = base_cli();
        let format = resolve_render_format(&cli, &PathBuf::from("out.mp4")).unwrap();
        assert_eq!(format, RenderFormat::Mpeg4);
    }

    #[test]
    fn parses_set_with_keyword() {
        let (tag, vr, value) =
            parse_attribute_override("SOPClassUID=1.2.840.10008.5.1.4.1.1.2").unwrap();
        assert_eq!(tag, Tag(0x0008, 0x0016));
        assert_eq!(vr, dicom_core::VR::UI);
        assert_eq!(value, "1.2.840.10008.5.1.4.1.1.2");
    }

    #[test]
    fn rejects_set_without_separator() {
        let error = parse_attribute_override("SOPClassUID").unwrap_err().to_string();
        assert!(error.contains("expected KEY=VALUE"));
    }

    #[test]
    fn parses_multiple_set_values_with_stdin_paths_flag() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dcmnorm",
                "-I",
                "--overwrite",
                "--set",
                "SOPClassUID=1.2.840.10008.5.1.4.1.1.2",
                "--set",
                "StudyDescription=Normalized",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();

        assert!(cli.stdin_paths);
        assert!(cli.overwrite);
        assert_eq!(cli.set.len(), 2);
        assert_eq!(cli.set[0], "SOPClassUID=1.2.840.10008.5.1.4.1.1.2");
        assert_eq!(cli.set[1], "StudyDescription=Normalized");
    }

    #[test]
    fn infers_overwrite_without_output_as_dicom_to_dicom() {
        let mut cli = base_cli();
        cli.overwrite = true;
        cli.set.push("SOPClassUID=1.2.840.10008.5.1.4.1.1.2".to_string());

        let mut input_bytes = vec![0u8; 132];
        input_bytes[128..132].copy_from_slice(b"DICM");

        let direction = infer_direction(&cli, &PathBuf::from("in.dcm"), &input_bytes).unwrap();
        assert_eq!(direction, Direction::DicomToDicom);
    }

    #[test]
    fn rejects_overwrite_with_explicit_output() {
        let mut cli = base_cli();
        cli.overwrite = true;
        cli.output = Some(PathBuf::from("out.dcm"));
        cli.set.push("SOPClassUID=1.2.840.10008.5.1.4.1.1.2".to_string());

        let mut input_bytes = vec![0u8; 132];
        input_bytes[128..132].copy_from_slice(b"DICM");

        let error = infer_direction(&cli, &PathBuf::from("in.dcm"), &input_bytes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be combined with an explicit output path"));
    }
}