use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{ArgAction, Parser, ValueEnum};
use dicom_dictionary_std::tags;
use dcmnorm::dicom_io::{
    jpeg2000_backend_name, list_transfer_syntax_support, read_dicom_bytes,
    read_dicom_json_with_options, render_all_dicom_frames, render_dicom_frame,
    transcode_dicom_object, write_dicom_file, write_dicom_json_with_options,
    DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    DicomJsonWriteOptions, RenderOutputFormat, RenderPipelineOptions,
};

#[derive(Parser, Debug)]
#[command(name = "dcmnorm")]
#[command(about = "Convert between DICOM and JSON")]
#[command(long_about = "Convert between DICOM and flattened or standard DICOM JSON. The CLI infers whether to run DICOM-to-JSON or JSON-to-DICOM from the input and output file types.")]
#[command(arg_required_else_help = true)]
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
        value_enum,
        help = "Render DICOM input to this format (raw/png/jpeg/mpeg4). For MPEG4 files, use the .mp4 output extension; if omitted, the format is inferred from the output extension"
    )]
    render_format: Option<RenderFormat>,

    #[arg(long, default_value_t = 0, help = "Zero-based frame index to render")]
    render_frame: usize,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Disable modality LUT during rendering"
    )]
    no_modality_lut: bool,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Disable VOI LUT / windowing during rendering"
    )]
    no_voi_lut: bool,

    #[arg(long, value_name = "FLOAT", help = "Override VOI window center for rendering")]
    window_center: Option<f64>,

    #[arg(long, value_name = "FLOAT", help = "Override VOI window width for rendering")]
    window_width: Option<f64>,

    #[arg(long, default_value_t = 90, help = "JPEG quality for rendered JPEG output (1-100)")]
    jpeg_quality: u8,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Render and export all frames for multiframe images. For image outputs, OUTPUT is expanded to STEM_000001.EXT, STEM_000002.EXT, and so on"
    )]
    render_all_frames: bool,

    #[arg(
        long,
        value_name = "FPS",
        help = "Frames per second when writing MPEG4/.mp4 output (defaults to DICOM frame rate metadata when available, else 24)"
    )]
    render_fps: Option<f64>,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Set the output width in pixels. If --output-height is also set, the image is scaled exactly; otherwise the height is computed from the aspect ratio"
    )]
    output_width: Option<u32>,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Set the output height in pixels. If --output-width is also set, the image is scaled exactly; otherwise the width is computed from the aspect ratio"
    )]
    output_height: Option<u32>,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Scale output while preserving aspect ratio so the longer side equals this value"
    )]
    scale_max_size: Option<u32>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "List transfer syntaxes known to this build and exit"
    )]
    list_transfer_syntaxes: bool,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Emit verbose conversion and rendering diagnostics"
    )]
    verbose: bool,
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
        Direction::DicomToRender => run_dicom_to_render(&cli, &input_bytes),
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
    validate_no_render_flags(cli)?;

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
    validate_no_render_flags(cli)?;

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

    let object = read_dicom_bytes(input_bytes)?;
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
        (None, FileKind::Dicom) => Ok(Direction::DicomToJson),
        (None, FileKind::Json) => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "JSON to DICOM conversion requires an output path",
        )
        .into()),
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
    use super::{detect_output_kind, resolve_render_format, Cli, FileKind, RenderFormat};
    use std::path::PathBuf;

    fn base_cli() -> Cli {
        Cli {
            input: None,
            output: None,
            format: super::JsonFormat::Flat,
            keys: super::KeyFormat::Name,
            bulk_data: super::BulkDataMode::Uri,
            bulk_data_source: None,
            transfer_syntax: None,
            render_format: None,
            render_frame: 0,
            no_modality_lut: false,
            no_voi_lut: false,
            window_center: None,
            window_width: None,
            jpeg_quality: 90,
            render_all_frames: false,
            render_fps: None,
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
}