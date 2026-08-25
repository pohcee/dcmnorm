use std::fs;
use std::io::{self, BufRead, ErrorKind, Write};
use std::io::Read;
use std::path::{Path, PathBuf};

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, ValueEnum};
use dcmnorm::dicom_io::{
    apply_filter_to_object, compute_frame_histogram, compute_instance_histograms, jpeg2000_backend_name,
    kakadu_ffi_enabled, list_transfer_syntax_support,
    parse_attribute_override, parse_filter_requests, parse_tag_key, read_dicom_bytes, read_dicom_file,
    read_dicom_json_with_options, read_dicom_object_for_filter,
    redact_dicom_pixels_to_transfer_syntax, remove_attribute, render_all_dicom_frames,
    render_dicom_frame, set_attribute, transcode_dicom_object, write_dicom_file,
    write_dicom_json_with_options, write_dicom_video, BoundingBox, BoxLength, DicomJsonBulkDataMode, DicomJsonFormat,
    DicomJsonKeyStyle, DicomJsonReadOptions, DicomJsonWriteOptions,
    RenderOutputFormat, RenderPipelineOptions, HistogramOptions, JPEG2000_CODEC_ENV_FLAG, JPEG2000_DEBUG_ENV_FLAG,
    probe_dicom_file_for_sop_class_uid,
    build_volume, canonical_view_basis, generate_uid, reformat_plane, reformat_plane_values, rotate_basis,
    write_nifti, write_nrrd, write_reformatted_dicom_slice,
    pack_dicom_frame_texture, pack_volume_texture,
    Interpolation, PackedTexture, PlaneParams, SlabProjection, SliceGeometry, TextureCompression, Volume, VolumeGeometry,
};
use dcmnorm::remove_private_tags_inplace;
use dcmnorm::perf;
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::Tag;
use dicom_dictionary_std::tags;
use dicom_dictionary_std::StandardDataDictionary;
use sha2::{Digest, Sha256};
use serde_json::Value as JsonValue;

#[derive(Parser, Debug)]
#[command(name = "dcmnorm")]
#[command(version)]
#[command(about = "Convert, transcode, and render DICOM data")]
#[command(
    long_about = "Convert between DICOM and flattened or standard DICOM JSON, transcode DICOM transfer syntaxes, render DICOM frames to raw/PNG/JPEG/MPEG4 outputs, and list transfer-syntax support for the current build. The CLI infers the operation from the input and output file types unless an explicit mode flag is provided."
)]
#[command(
    after_help = "Environment:\n  DCMNORM_PERF            Enable scoped perf logs (1/true/yes/on)\n  DCMNORM_JPEG2000_CODEC JPEG 2000 decoder preference: auto|openjpeg|kakadu\n                         (set by --jpeg2000-codec)\n  DCMNORM_JPEG2000_DEBUG Enable JPEG 2000 debug logs (1/true/yes/on)\n                         (set to 1 by --verbose)\n  LD_LIBRARY_PATH         Used to discover Kakadu libkdu*.so at runtime\n\nBuild-time Kakadu variables (for --features kakadu-ffi):\n  KAKADU_INCLUDE_DIR      Path containing Kakadu headers\n  KAKADU_LIB_DIR          Path containing libkdu*.so\n  KAKADU_LIB_NAME         Optional Kakadu library base name override"
)]
#[command(arg_required_else_help = true)]
struct Cli {
    // A single variadic positional, not two separate INPUT/OUTPUT fields - clap requires a
    // trailing positional after a variadic one to be `required`, which would break the
    // single-file "no OUTPUT means print JSON to stdout" convenience every command relies on.
    // Splitting INPUT(s) vs OUTPUT out of this list happens in application code instead (see
    // Cli::finalize, and run()'s own dispatch for the 3+/--mpr cases) - the same approach
    // `cp SOURCE... DEST`-style tools use, adapted to also allow 0 or 1 paths (unlike `cp`, which
    // always requires a destination).
    #[arg(
        value_name = "INPUT... [OUTPUT]",
        num_args = 0..,
        help = "One INPUT file (OUTPUT optional, defaults to stdout JSON), INPUT and OUTPUT (2 paths), or - for batch/--mpr - multiple files: 3+ paths alone batch-process each independently; with --mpr, all but the last combine into one volume and the last is OUTPUT. Shell globs work as usual",
        help_heading = "General",
        display_order = 1
    )]
    paths: Vec<PathBuf>,

    // Derived from `paths` by Cli::finalize() right after parsing - matches the original
    // single-file INPUT/OUTPUT positional fields exactly for 0-2 paths, so every existing
    // single-file call site below (the vast majority of this file) keeps working unchanged.
    // Multi-input dispatch (3+ paths, or --mpr) is handled separately in run(), reading `paths`
    // directly instead.
    #[arg(skip)]
    input: Option<PathBuf>,
    #[arg(skip)]
    output: Option<PathBuf>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Remove all private tags (odd group or element >= 0x0010)",
        help_heading = "DICOM Editing",
        display_order = 23
    )]
    remove_private_tags: bool,

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
        help = "Check whether INPUT is a valid DICOM file and print matching paths",
        help_heading = "General",
        display_order = 4
    )]
    check_dicom: bool,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Emit verbose conversion and rendering diagnostics",
        help_heading = "General",
        display_order = 5
    )]
    verbose: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = Jpeg2000Codec::Auto,
        help = "Force JPEG2000 decoder selection (auto/openjpeg/kakadu). Useful for codec A/B testing",
        help_heading = "General",
        display_order = 5
    )]
    jpeg2000_codec: Jpeg2000Codec,

    #[arg(
        short = 'I',
        long = "stdin-paths",
        action = ArgAction::SetTrue,
        help = "Read input paths from stdin, one per line (e.g. find . -name '*.dcm' | dcmnorm -I)",
        help_heading = "General",
        display_order = 6
    )]
    stdin_paths: bool,

    #[arg(
        long,
        value_name = "KEY",
        action = ArgAction::Append,
        value_delimiter = ',',
        num_args = 1,
        help = "Filter DICOM by keyword/tag (repeat or comma-separate, e.g. --filter StudyInstanceUID,PatientID). Only filtered elements are parsed/output for DICOM input",
        help_heading = "General",
        display_order = 7
    )]
    filter: Vec<String>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Overwrite input file in place",
        help_heading = "General",
        display_order = 7
    )]
    overwrite: bool,

    #[arg(
        long,
        value_enum,
        help = "Override input type detection (dicom or json). Allows processing files without or with misleading extensions",
        help_heading = "General",
        display_order = 8
    )]
    input_type: Option<InputType>,

    #[arg(
        long,
        value_enum,
        help = "Override output type detection (dicom, json, raw, png, jpeg, mpeg4, texture). For render formats (raw/png/jpeg/mpeg4/texture), extensions .mp4/.m4v/.mpeg4/.mov are inferred as mpeg4 and .gputex as texture",
        help_heading = "General",
        display_order = 9
    )]
    output_type: Option<OutputType>,

    #[arg(
        long,
        value_enum,
        default_value_t = JsonFormat::Flat,
        help = "JSON format: flat or standard",
        help_heading = "JSON Conversion",
        display_order = 10
    )]
    format: JsonFormat,

    #[arg(
        long,
        value_enum,
        default_value_t = KeyFormat::Name,
        help = "JSON object keys: name or hex tag",
        help_heading = "JSON Conversion",
        display_order = 11
    )]
    keys: KeyFormat,

    #[arg(
        long,
        value_enum,
        default_value_t = BulkDataMode::Uri,
        help = "Bulk data encoding: inline or uri (>32 bytes). Use --bulk-data-source to resolve or embed file:// URIs",
        help_heading = "JSON Conversion",
        display_order = 12
    )]
    bulk_data: BulkDataMode,

    #[arg(
        long,
        value_name = "SOURCE",
        num_args = 0..=1,
        default_missing_value = "",
        help = "For JSON-to-DICOM: path to resolve BulkDataURIs. For DICOM-to-JSON: omit value to embed input path as file://",
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
        help = "Set or replace element: KEY as keyword (e.g. SOPClassUID) or tag (0008,0016). Repeat for multiple",
        help_heading = "DICOM Editing",
        display_order = 21
    )]
    set: Vec<String>,

    #[arg(
        long,
        value_name = "KEY",
        action = ArgAction::Append,
        help = "Remove element: KEY as keyword (e.g. PatientName) or tag (0010,0010). Repeat for multiple",
        help_heading = "DICOM Editing",
        display_order = 22
    )]
    remove: Vec<String>,

    #[arg(
        long,
        default_value_t = 0,
        help = "Zero-based frame index to render",
        help_heading = "Rendering",
        display_order = 30
    )]
    render_frame: usize,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Export all frames with OUTPUT expanded to STEM_NNNNNN.EXT",
        help_heading = "Rendering",
        display_order = 32
    )]
    render_all_frames: bool,

    #[arg(
        long,
        value_name = "FPS",
        help = "Frames per second for MPEG4 (defaults to DICOM metadata, else 24)",
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
        action = ArgAction::SetTrue,
        help = "Disable applying embedded ICC profile during RGB rendering",
        help_heading = "Rendering",
        display_order = 36
    )]
    no_icc_profile: bool,

    #[arg(
        long,
        value_name = "FLOAT",
        help = "Override VOI window center for rendering",
        help_heading = "Rendering",
        display_order = 37
    )]
    window_center: Option<f64>,

    #[arg(
        long,
        value_name = "FLOAT",
        help = "Override VOI window width for rendering",
        help_heading = "Rendering",
        display_order = 38
    )]
    window_width: Option<f64>,

    #[arg(
        long,
        default_value_t = 90,
        help = "JPEG quality for rendered JPEG output (1-100)",
        help_heading = "Rendering",
        display_order = 39
    )]
    jpeg_quality: u8,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Output width; if height is set, scale exactly; else preserve aspect ratio",
        help_heading = "Rendering",
        display_order = 40
    )]
    output_width: Option<u32>,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Output height; if width is set, scale exactly; else preserve aspect ratio",
        help_heading = "Rendering",
        display_order = 41
    )]
    output_height: Option<u32>,

    #[arg(
        long,
        value_name = "PIXELS",
        help = "Scale output while preserving aspect ratio so the longer side equals this value",
        help_heading = "Rendering",
        display_order = 42
    )]
    scale_max_size: Option<u32>,

    #[arg(
        long,
        value_name = "X,Y,W,H",
        action = ArgAction::Append,
        allow_hyphen_values = true,
        help = "Add redaction box at X,Y (in output pixels) with size W×H. Negative coords anchor from right/bottom. W/H as pixels or %%. Repeat for multiple",
        help_heading = "Rendering",
        display_order = 43
    )]
    redact_box: Vec<String>,

    #[arg(
        long,
        value_name = "COLOR",
        help = "Fill color for redaction boxes as R,G,B (0-255 each) or #RRGGBB hex. Defaults to 0,0,0 (black)",
        help_heading = "Rendering",
        display_order = 44
    )]
    redact_color: Option<String>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Pad the output image to a square canvas",
        help_heading = "Rendering",
        display_order = 45
    )]
    pad: bool,

    #[arg(
        long,
        value_name = "COLOR",
        help = "Pad color for square canvas as R,G,B (0-255 each) or #RRGGBB hex. Defaults to 0,0,0 (black)",
        help_heading = "Rendering",
        display_order = 46
    )]
    pad_color: Option<String>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Do not render DICOM overlay planes, even if present",
        help_heading = "Rendering",
        display_order = 47
    )]
    no_overlays: bool,

    #[arg(
        long,
        value_name = "N",
        help = "Zero-based index of the overlay plane to render, when multiple are present. Defaults to the first available overlay",
        help_heading = "Rendering",
        display_order = 48
    )]
    overlay_index: Option<usize>,

    #[arg(
        long,
        value_name = "COLOR",
        help = "Fill color for rendered overlay pixels as R,G,B (0-255 each) or #RRGGBB hex. Defaults to 0,255,0 (green)",
        help_heading = "Rendering",
        display_order = 49
    )]
    overlay_color: Option<String>,

    #[arg(
        long,
        value_name = "axial|coronal|sagittal|YAW,PITCH,ROLL",
        allow_hyphen_values = true,
        help = "Render a Multiplanar Reformation: combine multiple DICOM slice files (positional args, or piped via -I/--stdin-paths) into one volume and render a single reformatted OUTPUT, instead of the default behavior of processing each path independently. Value is either a canonical patient-anatomy-aligned view (axial reuses the volume's own acquisition orientation; coronal/sagittal are fixed planes) or an arbitrary oblique rotation in degrees (YAW,PITCH,ROLL, about the patient's Z/X/Y axes respectively) applied to the native axial basis - not both combined",
        help_heading = "MPR (Multiplanar Reformation)",
        display_order = 50
    )]
    mpr: Option<String>,

    #[arg(
        long,
        value_name = "X,Y,Z",
        allow_hyphen_values = true,
        help = "Reformat plane center, in patient/LPS millimeters. Defaults to the built volume's own physical center. For most uses, --mpr-depth (a single offset along the plane's own normal) is more convenient than specifying an absolute point here",
        help_heading = "MPR (Multiplanar Reformation)",
        display_order = 54
    )]
    mpr_origin: Option<String>,

    #[arg(
        long,
        value_name = "MM|START:END[:STEP]|all[:STEP]",
        allow_hyphen_values = true,
        help = "Move the reformat plane along its own normal from --mpr-origin (or the volume's center). A single MM offset (default 0) reformats one plane, exactly as before. A START:END range (optionally with an explicit :STEP) instead produces a STACK of slices spanning that depth range - written as multiple numbered output files (.png/.jpg/.dcm) or as one whole-volume file (.nii/.nii.gz/.nrrd). 'all' spans the volume's own full extent along the plane's normal. The step defaults to --mpr-thickness (contiguous, non-overlapping slabs) if set, else --mpr-spacing",
        help_heading = "MPR (Multiplanar Reformation)",
        display_order = 55
    )]
    mpr_depth: Option<String>,

    #[arg(
        long,
        value_name = "MM",
        help = "Physical size of one output pixel, in mm (the same in both output axes, so the reformat is never distorted). Defaults to the volume's own smallest voxel dimension",
        help_heading = "MPR (Multiplanar Reformation)",
        display_order = 56
    )]
    mpr_spacing: Option<f64>,

    #[arg(
        long,
        value_name = "MM",
        help = "Reformat a thick slab instead of an infinitely-thin plane: samples multiple depths spanning this many millimeters (centered on the plane) and combines them per --mpr-projection. Defaults to 0 (thin plane, the original single-voxel-thick MPR behavior)",
        help_heading = "MPR (Multiplanar Reformation)",
        display_order = 57
    )]
    mpr_thickness: Option<f64>,

    #[arg(
        long,
        value_name = "mip|minip|average",
        help = "How --mpr-thickness's slab samples combine into one pixel: mip (maximum intensity projection - the radiology default, makes bright structures like vessels visible across the slab), minip (minimum intensity projection), or average. Requires --mpr-thickness. Defaults to mip",
        help_heading = "MPR (Multiplanar Reformation)",
        display_order = 58
    )]
    mpr_projection: Option<String>,

    #[arg(
        long,
        value_name = "N",
        help = "Cap the longest axis of a texture-format (.gputex / --output-type texture) export at N samples, proportionally downsampling if the source exceeds it. Defaults to no cap (full native resolution)",
        help_heading = "Texture Export",
        display_order = 60
    )]
    texture_max_dim: Option<u32>,

    #[arg(
        long,
        value_enum,
        help = "Compression applied to a texture-format export's raw payload bytes. Defaults to gzip",
        help_heading = "Texture Export",
        display_order = 61
    )]
    texture_compression: Option<TextureCompressionArg>,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Compute a pixel-value histogram (one per frame) and print it as JSON to OUTPUT or stdout, instead of the default DICOM/JSON conversion",
        help_heading = "Histogram",
        display_order = 70
    )]
    histogram: bool,

    #[arg(
        long,
        default_value_t = 256,
        help = "Number of bins per frame histogram",
        help_heading = "Histogram",
        display_order = 71
    )]
    histogram_bins: u32,

    #[arg(
        long,
        value_name = "N",
        help = "Compute only this zero-based frame's histogram, instead of every frame in the instance",
        help_heading = "Histogram",
        display_order = 72
    )]
    histogram_frame: Option<usize>,

    #[arg(
        long,
        value_name = "FLOAT",
        help = "Lower bound of the binned value range. Defaults to each frame's own observed minimum. Requires --histogram-max",
        help_heading = "Histogram",
        display_order = 73
    )]
    histogram_min: Option<f64>,

    #[arg(
        long,
        value_name = "FLOAT",
        help = "Upper bound of the binned value range. Defaults to each frame's own observed maximum. Requires --histogram-min",
        help_heading = "Histogram",
        display_order = 74
    )]
    histogram_max: Option<f64>,
}

impl Cli {
    /// Derives the legacy single-file `input`/`output` fields from `paths`, matching the
    /// original two-positional behavior exactly for 0-2 paths (every single-file call site below
    /// keeps reading `cli.input`/`cli.output` and needs no changes). 3+ paths, and --mpr's own
    /// 2+-path convention, are deliberately NOT handled here - those are multi-input cases with
    /// no single "the" output the way a single-file command has, so run() reads `cli.paths`
    /// directly for them instead of going through this derivation.
    fn finalize(mut self) -> Self {
        self.input = self.paths.first().cloned();
        self.output = if self.paths.len() == 2 { self.paths.get(1).cloned() } else { None };
        self
    }

    /// Whether MPR mode is in effect - `--mpr <axial|coronal|sagittal|YAW,PITCH,ROLL>` is the
    /// sole trigger now that view/rotation are folded into it. `--mpr-origin`/`--mpr-spacing`
    /// remain separate (they're numeric refinements, not "which plane" choices), but only mean
    /// anything alongside `--mpr` - see the validation in run() that rejects them without it,
    /// rather than silently ignoring them.
    fn mpr_requested(&self) -> bool {
        self.mpr.is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum JsonFormat {
    Flat,
    Standard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum InputType {
    Dicom,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputType {
    Dicom,
    Json,
    Raw,
    Png,
    Jpeg,
    Mpeg4,
    Texture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TextureCompressionArg {
    None,
    Gzip,
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
enum RenderFormat {
    Raw,
    Png,
    Jpeg,
    Mpeg4,
    Texture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Jpeg2000Codec {
    Auto,
    Openjpeg,
    Kakadu,
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

const NEGATIVE_ZERO_ANCHOR: i32 = i32::MIN;

fn main() {
    if let Err(error) = run() {
        if !error.to_string().is_empty() {
            eprintln!("{error}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _scope = perf::scope("cli.run");
    let version_with_hash = cli_version_with_binary_hash();
    let version_static: &'static str = Box::leak(version_with_hash.into_boxed_str());
    let matches = Cli::command().version(version_static).get_matches();
    let cli = Cli::from_arg_matches(&matches).expect("clap generated invalid matches").finalize();

    if cli.jpeg2000_codec == Jpeg2000Codec::Kakadu && !kakadu_ffi_enabled() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--jpeg2000-codec kakadu requested, but this build does not include kakadu-ffi",
        )
        .into());
    }

    let codec_env_value = match cli.jpeg2000_codec {
        Jpeg2000Codec::Auto => "auto",
        Jpeg2000Codec::Openjpeg => "openjpeg",
        Jpeg2000Codec::Kakadu => "kakadu",
    };
    std::env::set_var(JPEG2000_CODEC_ENV_FLAG, codec_env_value);

    if cli.verbose {
        std::env::set_var(JPEG2000_DEBUG_ENV_FLAG, "1");
    }

    if cli.list_transfer_syntaxes {
        print_transfer_syntax_support()?;
        return Ok(());
    }

    if cli.check_dicom {
        validate_check_dicom_flags(&cli)?;
        return run_check_dicom(&cli);
    }

    if cli.histogram {
        validate_histogram_flags(&cli)?;
        return run_histogram(&cli);
    }

    if !cli.mpr_requested()
        && (cli.mpr_origin.is_some()
            || cli.mpr_depth.is_some()
            || cli.mpr_spacing.is_some()
            || cli.mpr_thickness.is_some()
            || cli.mpr_projection.is_some())
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--mpr-origin/--mpr-depth/--mpr-spacing/--mpr-thickness/--mpr-projection require --mpr <axial|coronal|sagittal|YAW,PITCH,ROLL>",
        )
        .into());
    }

    if cli.mpr_projection.is_some() && cli.mpr_thickness.is_none() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--mpr-projection requires --mpr-thickness",
        )
        .into());
    }

    // Multiple inputs come from two interchangeable places - the trailing positional PATHS
    // (shell-glob-friendly, e.g. `dcmnorm a.dcm b.dcm c.dcm out.png`) or piped one-per-line via
    // -I/--stdin-paths (better suited to a huge/programmatically-generated list, e.g. `find`).
    // Both read/collect the ENTIRE file set, sequentially, inside this one process before doing
    // anything with it - nothing here spawns a second dcmnorm process or hands paths off
    // elsewhere, so a series can never get silently split across separate volumes this way. (The
    // one way to actually cause that would be piping through `xargs` instead of straight into
    // `-I` - xargs batches long argument lists across multiple subprocess invocations, each
    // seeing only a slice of the files and running as its own independent `dcmnorm` process with
    // its own empty stdin, so it would fail loudly with a "requires ..." error rather than
    // silently building a partial/wrong volume - but that's a caller misuse, not something -I
    // itself does.)
    //
    // With -I, stdin supplies every INPUT - at most one trailing positional is allowed, and (only
    // meaningful with --mpr, which is the only multi-input mode with a combined OUTPUT) it's
    // OUTPUT, not another input.
    if cli.stdin_paths {
        if cli.paths.len() > 1 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "with -I/--stdin-paths, at most one extra positional (OUTPUT, for --mpr) is allowed - every INPUT comes from stdin",
            )
            .into());
        }
        let inputs = read_stdin_paths()?;
        if cli.mpr_requested() {
            let output_path = cli.paths.first().ok_or_else(|| {
                io::Error::new(
                    ErrorKind::InvalidInput,
                    "--mpr requires an output path, e.g. find series_dir -name '*.dcm' | dcmnorm -I --mpr out.png",
                )
            })?;
            return run_mpr(&cli, &inputs, output_path);
        }
        return run_batch(&cli, &inputs);
    }

    // Without -I, the trailing positional PATHS are split by application-level convention (see
    // Cli::finalize's own doc comment for the 0-2 case, handled below via cli.input/cli.output):
    // --mpr treats all but the last as INPUTs and the last as the combined OUTPUT (requires 2+);
    // otherwise, 3+ paths batch-process each independently (no shared output, same as -I without
    // --mpr); 0-2 paths keep the exact original single-file input/[output] behavior.
    if cli.mpr_requested() {
        if cli.paths.len() < 2 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "--mpr requires multiple slice files and an output path, e.g. dcmnorm --mpr series_dir/*.dcm out.png",
            )
            .into());
        }
        let (inputs, output_path) = cli.paths.split_at(cli.paths.len() - 1);
        return run_mpr(&cli, inputs, &output_path[0]);
    }

    if cli.paths.len() >= 3 {
        return run_batch(&cli, &cli.paths);
    }

    let input_path = cli.input.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "an input path is required unless --list-transfer-syntaxes is set",
        )
    })?;

    process_one(&cli, input_path)
}

fn read_stdin_paths() -> io::Result<Vec<PathBuf>> {
    let stdin = io::stdin();
    let mut paths = Vec::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        paths.push(PathBuf::from(line));
    }
    Ok(paths)
}

// Processes every path independently (JSON/DICOM/render, whichever `cli`'s other flags select) -
// shared by both ways of supplying multiple non-MPR inputs (3+ positional paths and
// -I/--stdin-paths).
fn run_batch(cli: &Cli, paths: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let mut any_error = false;
    for input_path in paths {
        if let Err(e) = process_one(cli, input_path) {
            eprintln!("{}: {e}", input_path.display());
            any_error = true;
        }
    }
    if any_error {
        return Err(io::Error::new(ErrorKind::Other, "one or more inputs failed").into());
    }
    Ok(())
}

fn run_check_dicom(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();

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
            let Ok(metadata) = fs::metadata(&input_path) else {
                any_error = true;
                continue;
            };

            if !metadata.is_file() {
                any_error = true;
                continue;
            }

            match probe_dicom_file_for_sop_class_uid(&input_path) {
                Ok(true) => {
                    writeln!(stdout, "{}", input_path.display())?;
                }
                Ok(false) => {
                    any_error = true;
                }
                Err(error) => {
                    let _ = error;
                    any_error = true;
                }
            }
        }

        if any_error {
            return Err(io::Error::new(ErrorKind::InvalidInput, "").into());
        }
        return Ok(());
    }

    let input_path = cli.input.as_ref().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "an input path is required for --check-dicom",
        )
    })?;

    match probe_dicom_file_for_sop_class_uid(input_path) {
        Ok(true) => {
            writeln!(stdout, "{}", input_path.display())?;
            Ok(())
        }
        Ok(false) | Err(_) => Err(io::Error::new(ErrorKind::InvalidInput, "").into()),
    }
}

fn infer_direction_for_filter(
    cli: &Cli,
    input: &Path,
) -> Result<Direction, Box<dyn std::error::Error>> {
    let input_kind = detect_input_kind_for_filter(input, cli.input_type)?;
    if input_kind != FileKind::Dicom {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--filter is only supported for DICOM input",
        )
        .into());
    }

    if cli.overwrite && cli.output.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--overwrite cannot be combined with an explicit output path",
        )
        .into());
    }

    match &cli.output {
        Some(output) => match detect_output_kind(output, cli.output_type) {
            Some(FileKind::Json) => Ok(Direction::DicomToJson),
            Some(FileKind::Dicom) => Ok(Direction::DicomToDicom),
            Some(FileKind::Render) => Ok(Direction::DicomToRender),
            None => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "could not determine output type; use .json, .dcm/.dicom, or a render extension (.jpg/.jpeg/.png/.raw/.mp4/.m4v/.mpeg4/.mov), or use --output-type",
            )
            .into()),
        },
        None => {
            if cli.overwrite {
                Ok(Direction::DicomToDicom)
            } else {
                Ok(Direction::DicomToJson)
            }
        }
    }
}

fn detect_input_kind_for_filter(
    path: &Path,
    explicit_type: Option<InputType>,
) -> Result<FileKind, Box<dyn std::error::Error>> {
    if let Some(input_type) = explicit_type {
        return Ok(match input_type {
            InputType::Dicom => FileKind::Dicom,
            InputType::Json => FileKind::Json,
        });
    }

    if let Some(kind) = detect_kind_from_extension(path) {
        if kind != FileKind::Render {
            return Ok(kind);
        }
    }

    if probe_dicom_file_for_sop_class_uid(path).unwrap_or(false) {
        return Ok(FileKind::Dicom);
    }

    let mut file = fs::File::open(path)?;
    let mut head = vec![0u8; 4096];
    let read = file.read(&mut head)?;
    head.truncate(read);

    if looks_like_json(&head) {
        return Ok(FileKind::Json);
    }

    Err(io::Error::new(
        ErrorKind::InvalidInput,
        "could not determine input type; use a .json, .dcm, or .dicom extension, or use --input-type",
    )
    .into())
}

fn process_one(cli: &Cli, input_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let _scope = perf::scope("cli.process_one");

    if !cli.filter.is_empty() {
        let requests = parse_filter_requests(&cli.filter)?;
        let direction = infer_direction_for_filter(cli, input_path)?;
        // For URI bulk-data JSON output, parse from full bytes so PixelData can
        // keep source mapping needed for BulkDataURI emission.
        let (mut object, filter_input_bytes) = if direction == Direction::DicomToJson
            && cli.bulk_data == BulkDataMode::Uri
        {
            let input_bytes = fs::read(input_path)?;
            (read_dicom_bytes(&input_bytes)?, Some(input_bytes))
        } else {
            (read_dicom_object_for_filter(input_path, &requests)?, None)
        };

        return match direction {
            Direction::DicomToJson => run_dicom_to_json_with_object(
                cli,
                input_path,
                filter_input_bytes.as_deref(),
                object,
            ),
            Direction::DicomToDicom => {
                apply_filter_to_object(&mut object, &requests);
                run_dicom_to_dicom_with_object(cli, input_path, object)
            }
            Direction::DicomToRender => {
                apply_filter_to_object(&mut object, &requests);
                run_dicom_to_render_with_object(cli, object)
            }
            Direction::JsonToDicom => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "--filter is only supported for DICOM input",
            )
            .into()),
        };
    }

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
    let object = {
        let _parse_scope = perf::scope("cli.dicom_to_json.read_dicom_bytes");
        read_dicom_bytes(input_bytes)?
    };

    run_dicom_to_json_with_object(cli, input_path, Some(input_bytes), object)
}

fn run_dicom_to_json_with_object(
    cli: &Cli,
    input_path: &Path,
    input_bytes: Option<&[u8]>,
    mut object: dicom_object::DefaultDicomObject,
) -> Result<(), Box<dyn std::error::Error>> {
    let _scope = perf::scope("cli.run_dicom_to_json");
    validate_no_render_or_redaction_flags(cli)?;

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

    apply_attribute_overrides(cli, &mut object)?;
    if cli.remove_private_tags {
        remove_private_tags_inplace(&mut object);
        verbose_log(cli, "Removed all private tags from DICOM object");
    }
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
    let uri_base_owned: Option<String> = if bulk_data_mode == DicomJsonBulkDataMode::Uri
        && cli.bulk_data_source.as_deref() == Some("")
    {
        path_to_file_uri(input_path)
    } else {
        None
    };

    // Shared across every bulk-eligible element of this file's write pass - see
    // DicomJsonWriteOptions::bulk_scan_failed's own doc comment for why this matters (without
    // it, one element the hand-rolled offset scanner can't parse means every later one,
    // including PixelData, independently pays the same doomed multi-second scan instead of just
    // the first).
    let bulk_scan_failed = std::cell::Cell::new(false);
    let bulk_scan_cursor = std::cell::Cell::new(0usize);
    let mut output = {
        let _json_scope = perf::scope("cli.dicom_to_json.write_dicom_json_with_options");
        write_dicom_json_with_options(
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
            bulk_data_source: if bulk_data_mode == DicomJsonBulkDataMode::Uri { input_bytes } else { None },
            bulk_data_uri_base: uri_base_owned.as_deref(),
            bulk_scan_failed: Some(&bulk_scan_failed),
            bulk_scan_cursor: Some(&bulk_scan_cursor),
        },
    )?
    };

    if !cli.filter.is_empty() {
        let filter_requests = parse_filter_requests(&cli.filter)?;
        let keep_tags = filter_requests
            .iter()
            .map(|request| request.tag)
            .collect::<Vec<_>>();
        output = filter_json_output_to_tags(output, &keep_tags)?;
    }

    if let Some(path) = &cli.output {
        fs::write(path, output)?;
    } else {
        let mut stdout = io::stdout().lock();
        stdout.write_all(output.as_bytes())?;
        stdout.write_all(b"\n")?;
    }

    Ok(())
}

fn filter_json_output_to_tags(
    output: String,
    keep_tags: &[Tag],
) -> Result<String, Box<dyn std::error::Error>> {
    let mut value: JsonValue = serde_json::from_str(&output)?;
    let JsonValue::Object(ref mut map) = value else {
        return Ok(output);
    };

    map.retain(|key, _| {
        StandardDataDictionary
            .parse_tag(key)
            .map(|tag| keep_tags.contains(&tag))
            .unwrap_or(false)
    });

    Ok(serde_json::to_string(&value)?)
}

fn run_dicom_to_dicom(
    cli: &Cli,
    input_path: &Path,
    input_bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let object = {
        let _parse_scope = perf::scope("cli.dicom_to_dicom.read_dicom_bytes");
        read_dicom_bytes(input_bytes)?
    };

    run_dicom_to_dicom_with_object(cli, input_path, object)
}

fn run_dicom_to_dicom_with_object(
    cli: &Cli,
    input_path: &Path,
    mut object: dicom_object::DefaultDicomObject,
) -> Result<(), Box<dyn std::error::Error>> {
    let _scope = perf::scope("cli.run_dicom_to_dicom");
    validate_non_dicom_to_dicom_render_flags(cli)?;

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

    if cli.redact_color.is_some() && cli.redact_box.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--redact-color requires at least one --redact-box",
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

    apply_attribute_overrides(cli, &mut object)?;
    if cli.remove_private_tags {
        remove_private_tags_inplace(&mut object);
        verbose_log(cli, "Removed all private tags from DICOM object");
    }

    if !cli.redact_box.is_empty() {
        let target_transfer_syntax = cli
            .transfer_syntax
            .as_deref()
            .unwrap_or(object.meta().transfer_syntax());
        ensure_supported_redaction_target_transfer_syntax(target_transfer_syntax)?;

        let bounding_boxes = cli
            .redact_box
            .iter()
            .map(|s| parse_redact_box(s))
            .collect::<Result<Vec<_>, _>>()?;
        let bounding_box_color = cli
            .redact_color
            .as_deref()
            .map(parse_redact_color)
            .transpose()?
            .unwrap_or([0, 0, 0]);

        verbose_log(
            cli,
            format!(
                "Applying {} redaction box(es) and transcoding to transfer syntax {}",
                bounding_boxes.len(),
                target_transfer_syntax
            ),
        );
        let mut redacted = {
            let _redact_scope = perf::scope("cli.dicom_to_dicom.redact_pixels");
            redact_dicom_pixels_to_transfer_syntax(
            &object,
            target_transfer_syntax,
            &bounding_boxes,
            bounding_box_color,
        )?
        };
        write_dicom_file(&mut redacted, output_path)?;
        return Ok(());
    }

    if let Some(target_transfer_syntax) = target_transfer_syntax {
        verbose_log(
            cli,
            format!(
                "Transcoding DICOM to transfer syntax {} -> {}",
                object.meta().transfer_syntax(),
                target_transfer_syntax
            ),
        );
        let mut transcoded = {
            let _transcode_scope = perf::scope("cli.dicom_to_dicom.transcode");
            transcode_dicom_object(&object, target_transfer_syntax)?
        };
        write_dicom_file(&mut transcoded, output_path)?;
    } else {
        verbose_log(
            cli,
            format!("Writing updated DICOM to {}", output_path.display()),
        );
        write_dicom_file(&mut object, output_path)?;
    }

    Ok(())
}

fn run_dicom_to_render(cli: &Cli, input_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let object = {
        let _parse_scope = perf::scope("cli.dicom_to_render.read_dicom_bytes");
        read_dicom_bytes(input_bytes)?
    };

    run_dicom_to_render_with_object(cli, object)
}

fn run_dicom_to_render_with_object(
    cli: &Cli,
    mut object: dicom_object::DefaultDicomObject,
) -> Result<(), Box<dyn std::error::Error>> {
    let _scope = perf::scope("cli.run_dicom_to_render");
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

    verbose_log(
        cli,
        format!(
            "JPEG2000 backend for this build: {}",
            jpeg2000_backend_name()
        ),
    );
    apply_attribute_overrides(cli, &mut object)?;
    let format = resolve_render_format(cli, output_path)?;

    // Texture export bypasses the whole windowed/overlay/redaction 2D-render pipeline below
    // (it wants raw, unwindowed physical values - see dicom_io::texture_export's module doc) -
    // handled here, early, rather than threaded through RenderPipelineOptions.
    if format == RenderFormat::Texture {
        for (flag, present) in [
            ("--render-all-frames", cli.render_all_frames),
            ("--render-fps", cli.render_fps.is_some()),
            ("--output-width", cli.output_width.is_some()),
            ("--output-height", cli.output_height.is_some()),
            ("--scale-max-size", cli.scale_max_size.is_some()),
            ("--redact-box", !cli.redact_box.is_empty()),
        ] {
            if present {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{flag} is not valid with texture output - it always exports the frame's native resolution and raw physical values"),
                )
                .into());
            }
        }

        let default_window = match (cli.window_center, cli.window_width) {
            (Some(center), Some(width)) => Some((center, width)),
            _ => None,
        };
        let compression = resolve_texture_compression(cli);
        let packed = {
            let _scope = perf::scope("cli.dicom_to_render.pack_dicom_frame_texture");
            pack_dicom_frame_texture(&object, cli.render_frame, cli.texture_max_dim, default_window, compression)
                .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?
        };
        verbose_log(
            cli,
            format!(
                "Packed frame texture: {}x{} lossless={} ({} bytes stored, {} bytes raw)",
                packed.meta.width, packed.meta.height, packed.meta.lossless, packed.meta.payload_bytes_stored, packed.meta.payload_bytes_raw
            ),
        );
        write_packed_texture(output_path, &packed)?;
        return Ok(());
    }

    verbose_log(
        cli,
        format!(
            "Rendering DICOM to {:?} (output={}, frame={}, all_frames={}, modality_lut={}, voi_lut={}, jpeg_quality={}, show_overlays={}, overlay_index={:?})",
            format,
            output_path.display(),
            cli.render_frame,
            cli.render_all_frames || format == RenderFormat::Mpeg4,
            !cli.no_modality_lut,
            !cli.no_voi_lut,
            cli.jpeg_quality,
            !cli.no_overlays,
            cli.overlay_index
        ),
    );
    let bounding_boxes = cli
        .redact_box
        .iter()
        .map(|s| parse_redact_box(s))
        .collect::<Result<Vec<_>, _>>()?;
    let bounding_box_color = cli
        .redact_color
        .as_deref()
        .map(parse_redact_color)
        .transpose()?
        .unwrap_or([0, 0, 0]);

    if let Some(max_dim) = cli.scale_max_size {
        if max_dim > 65535 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "--scale-max-size cannot exceed 65535 (max DICOM resolution)",
            )
            .into());
        }
    }

    if let Some(w) = cli.output_width {
        if w > 65535 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "--output-width cannot exceed 65535 (max DICOM resolution)",
            )
            .into());
        }
    }

    if let Some(h) = cli.output_height {
        if h > 65535 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "--output-height cannot exceed 65535 (max DICOM resolution)",
            )
            .into());
        }
    }

    let pad_color = cli
        .pad_color
        .as_deref()
        .map(parse_redact_color)
        .transpose()?
        .unwrap_or([0, 0, 0]);

    if cli.pad && cli.scale_max_size.is_none() {
        return Err(
            io::Error::new(ErrorKind::InvalidInput, "--pad requires --scale-max-size").into(),
        );
    }

    if cli.pad_color.is_some() && !cli.pad {
        return Err(io::Error::new(ErrorKind::InvalidInput, "--pad-color requires --pad").into());
    }

    if cli.overlay_index.is_some() && cli.no_overlays {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--overlay-index cannot be combined with --no-overlays",
        )
        .into());
    }

    if cli.overlay_color.is_some() && cli.no_overlays {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--overlay-color cannot be combined with --no-overlays",
        )
        .into());
    }

    let overlay_color = cli
        .overlay_color
        .as_deref()
        .map(parse_redact_color)
        .transpose()?
        .unwrap_or([0, 255, 0]);

    let options = RenderPipelineOptions {
        frame_index: cli.render_frame,
        apply_modality_lut: !cli.no_modality_lut,
        apply_voi_lut: !cli.no_voi_lut,
        apply_icc_profile: !cli.no_icc_profile,
        window_center: cli.window_center,
        window_width: cli.window_width,
        jpeg_quality: cli.jpeg_quality,
        output_width: cli.output_width,
        output_height: cli.output_height,
        scale_max_size: cli.scale_max_size,
        bounding_boxes,
        bounding_box_color,
        pad: cli.pad,
        pad_color,
        show_overlays: !cli.no_overlays,
        overlay_index: cli.overlay_index,
        overlay_color,
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
        {
            let _mpeg_scope = perf::scope("cli.dicom_to_render.write_mpeg4");
            write_dicom_video(&object, output_path, &options, fps)?;
        }
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
        let rendered = {
            let _render_scope = perf::scope("cli.dicom_to_render.render_all_frames");
            render_all_dicom_frames(&object, to_render_output_format(format), &options)?
        };
        verbose_log(cli, format!("Rendered {} frame(s)", rendered.len()));
        write_multi_frame_outputs(output_path, format, rendered)?;
        return Ok(());
    }

    let rendered = {
        let _render_scope = perf::scope("cli.dicom_to_render.render_single_frame");
        render_dicom_frame(&object, to_render_output_format(format), &options)?
    };
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

/// `--mpr` entry point: builds one volume from `inputs`, resolves the requested cut plane
/// (`--mpr`'s own view-or-rotation value, plus `--mpr-origin`/`--mpr-spacing`, each defaulting to
/// a sane volume-derived value), and writes a single reformatted image to `output_path` - the same
/// `dicom_io::volume` functions the Node bindings call, so this is both a standalone tool and the
/// fastest way to exercise `volume.rs` without a render-server round trip.
fn run_mpr(cli: &Cli, inputs: &[PathBuf], output_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let _scope = perf::scope("cli.run_mpr");
    let paths = inputs;

    if paths.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--mpr requires at least one DICOM slice file as input",
        )
        .into());
    }

    for (flag, present) in [
        ("--filter", !cli.filter.is_empty()),
        ("--transfer-syntax", cli.transfer_syntax.is_some()),
        ("--set", !cli.set.is_empty()),
        ("--remove", !cli.remove.is_empty()),
        ("--render-all-frames", cli.render_all_frames),
        ("--render-fps", cli.render_fps.is_some()),
        ("--scale-max-size", cli.scale_max_size.is_some()),
    ] {
        if present {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{flag} is not valid with --mpr"),
            )
            .into());
        }
    }

    verbose_log(cli, format!("Building MPR volume from {} file(s)", paths.len()));
    let volume = build_volume(paths).map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?;
    verbose_log(
        cli,
        format!(
            "Volume: {}x{}x{} voxels, row spacing {:.4}mm, col spacing {:.4}mm, ~{:.4}mm min spacing",
            volume.cols,
            volume.rows,
            volume.num_slices,
            volume.row_spacing_mm,
            volume.col_spacing_mm,
            volume.min_spacing_mm()
        ),
    );

    // --mpr's value is either a canonical view keyword or a YAW,PITCH,ROLL rotation triplet
    // applied to the volume's own native axial basis - not both combined (see the flag's own
    // help text). cli.mpr is guaranteed Some here: run() only reaches run_mpr() once
    // mpr_requested() (== cli.mpr.is_some()) is true.
    let mpr_value = cli.mpr.as_deref().expect("run_mpr is only called once cli.mpr_requested()");
    let (row_dir, col_dir) = if matches!(mpr_value, "axial" | "coronal" | "sagittal") {
        canonical_view_basis(mpr_value, &volume).map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?
    } else {
        let (yaw, pitch, roll) = parse_triplet(mpr_value, "--mpr").map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "invalid --mpr value '{mpr_value}': expected 'axial', 'coronal', 'sagittal', or a YAW,PITCH,ROLL rotation triplet"
                ),
            )
        })?;
        rotate_basis(volume.row_vector, volume.col_vector, yaw, pitch, roll)
    };

    let base_origin = match cli.mpr_origin.as_deref() {
        Some(value) => {
            let (x, y, z) = parse_triplet(value, "--mpr-origin")?;
            [x, y, z]
        }
        None => volume.center(),
    };

    // The reformat plane's own normal (cross(row_dir, col_dir)) - --mpr-depth moves along this,
    // and it's also the "k" (slice/depth) axis for whole-volume/DICOM-series export.
    let normal_raw = [
        row_dir[1] * col_dir[2] - row_dir[2] * col_dir[1],
        row_dir[2] * col_dir[0] - row_dir[0] * col_dir[2],
        row_dir[0] * col_dir[1] - row_dir[1] * col_dir[0],
    ];
    let normal_len = (normal_raw[0].powi(2) + normal_raw[1].powi(2) + normal_raw[2].powi(2)).sqrt().max(1e-9);
    let normal = [normal_raw[0] / normal_len, normal_raw[1] / normal_len, normal_raw[2] / normal_len];

    let slab_thickness_mm = cli.mpr_thickness.unwrap_or(0.0);
    let slab_projection = match cli.mpr_projection.as_deref() {
        None | Some("mip") => SlabProjection::MaximumIntensity,
        Some("minip") => SlabProjection::MinimumIntensity,
        Some("average") => SlabProjection::Average,
        Some(other) => {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid --mpr-projection value '{other}': expected 'mip', 'minip', or 'average'"),
            )
            .into())
        }
    };

    let spacing_mm = cli.mpr_spacing.unwrap_or_else(|| volume.min_spacing_mm());
    if !(spacing_mm > 0.0) {
        return Err(io::Error::new(ErrorKind::InvalidInput, "--mpr-spacing must be greater than zero").into());
    }

    let depth_spec = match cli.mpr_depth.as_deref() {
        Some(value) => parse_depth_spec(value)?,
        None => DepthSpec::Single(0.0),
    };
    let default_depth_step = if slab_thickness_mm > 0.0 { slab_thickness_mm } else { spacing_mm };
    let depths = resolve_depths(depth_spec, &volume, row_dir, col_dir, base_origin, default_depth_step);
    if depths.is_empty() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "--mpr-depth resolved to zero slices").into());
    }

    let output_kind = resolve_mpr_output_kind(cli, output_path)?;
    if cli.output_type.is_some() && !matches!(output_kind, MprOutputKind::Rendered(_)) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--output-type is not valid with .nii/.nii.gz/.nrrd/.dcm --mpr output - format is determined by the output extension",
        )
        .into());
    }

    // A texture export ships the volume's own NATIVE voxel lattice (see
    // dicom_io::texture_export's module doc) - none of the plane/depth/spacing flags above
    // (already parsed by this point) apply, so reject them explicitly rather than silently
    // ignoring a value the user thought was taking effect.
    if matches!(output_kind, MprOutputKind::Rendered(RenderFormat::Texture)) {
        for (flag, present) in [
            ("--mpr-origin", cli.mpr_origin.is_some()),
            ("--mpr-depth", cli.mpr_depth.is_some()),
            ("--mpr-spacing", cli.mpr_spacing.is_some()),
            ("--mpr-thickness", cli.mpr_thickness.is_some()),
            ("--mpr-projection", cli.mpr_projection.is_some()),
            ("--output-width", cli.output_width.is_some()),
            ("--output-height", cli.output_height.is_some()),
        ] {
            if present {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{flag} is not valid with texture (.gputex) --mpr output - it always exports the whole native volume lattice, not a reformatted plane"),
                )
                .into());
            }
        }

        let default_window = match (cli.window_center, cli.window_width) {
            (Some(center), Some(width)) => Some((center, width)),
            _ => None,
        };
        let compression = resolve_texture_compression(cli);
        let packed = {
            let _scope = perf::scope("cli.run_mpr.pack_volume_texture");
            pack_volume_texture(&volume, cli.texture_max_dim, default_window, compression)
                .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?
        };
        verbose_log(
            cli,
            format!(
                "Packed volume texture: {}x{}x{} lossless={} downsampled={} ({} bytes stored, {} bytes raw)",
                packed.meta.width,
                packed.meta.height,
                packed.meta.depth,
                packed.meta.lossless,
                packed.meta.downsampled,
                packed.meta.payload_bytes_stored,
                packed.meta.payload_bytes_raw
            ),
        );
        write_packed_texture(output_path, &packed)?;
        return Ok(());
    }

    let output_width = cli.output_width.unwrap_or(volume.cols);
    let output_height = cli.output_height.unwrap_or(volume.rows);
    if output_width > 65535 || output_height > 65535 {
        return Err(io::Error::new(ErrorKind::InvalidInput, "--output-width/--output-height cannot exceed 65535").into());
    }

    verbose_log(
        cli,
        format!(
            "Reformatting {} slice(s): row_dir={row_dir:?} col_dir={col_dir:?} spacing={spacing_mm:.4}mm output={output_width}x{output_height}",
            depths.len()
        ),
    );

    let plane_params_at = |depth_mm: f64| PlaneParams {
        origin: [
            base_origin[0] + normal[0] * depth_mm,
            base_origin[1] + normal[1] * depth_mm,
            base_origin[2] + normal[2] * depth_mm,
        ],
        row_dir,
        col_dir,
        output_width,
        output_height,
        spacing_mm,
        window_center: cli.window_center,
        window_width: cli.window_width,
        interpolation: Interpolation::Trilinear,
        slab_thickness_mm,
        slab_projection,
    };

    // The world-space CENTER of voxel (col=0, row=0) of the plane at `depth_mm` - the same
    // "center of the first voxel" convention DICOM's own ImagePositionPatient uses, matching
    // exactly how reformat_plane_values indexes its output (see its own half_width/half_height
    // offset math).
    let first_voxel_center_at = |depth_mm: f64| {
        let params = plane_params_at(depth_mm);
        let half_width = output_width as f64 / 2.0;
        let half_height = output_height as f64 / 2.0;
        [
            params.origin[0] + row_dir[0] * (-half_width) * spacing_mm + col_dir[0] * (-half_height) * spacing_mm,
            params.origin[1] + row_dir[1] * (-half_width) * spacing_mm + col_dir[1] * (-half_height) * spacing_mm,
            params.origin[2] + row_dir[2] * (-half_width) * spacing_mm + col_dir[2] * (-half_height) * spacing_mm,
        ]
    };

    match output_kind {
        MprOutputKind::Rendered(format) => {
            if format == RenderFormat::Mpeg4 {
                return Err(io::Error::new(ErrorKind::InvalidInput, "--mpr does not support MPEG4 output").into());
            }
            if depths.len() == 1 {
                let params = plane_params_at(depths[0]);
                let rendered = reformat_plane(&volume, &params, to_render_output_format(format), cli.jpeg_quality)
                    .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?;
                fs::write(output_path, rendered.bytes)?;
            } else {
                let mut frames = Vec::with_capacity(depths.len());
                for depth_mm in &depths {
                    let params = plane_params_at(*depth_mm);
                    frames.push(
                        reformat_plane(&volume, &params, to_render_output_format(format), cli.jpeg_quality)
                            .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?,
                    );
                }
                write_multi_frame_outputs(output_path, format, frames)?;
            }
        }
        MprOutputKind::DicomSeries => {
            let source_object = read_dicom_file(&paths[0])?;
            let series_instance_uid = generate_uid();
            let slice_thickness_mm = if slab_thickness_mm > 0.0 { slab_thickness_mm } else { default_depth_step };

            if depths.len() == 1 {
                write_reformatted_dicom_slice(
                    &source_object,
                    &reformat_plane_values(&volume, &plane_params_at(depths[0]))
                        .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?,
                    output_height,
                    output_width,
                    &series_instance_uid,
                    1,
                    &SliceGeometry {
                        position: first_voxel_center_at(depths[0]),
                        row_dir,
                        col_dir,
                        row_spacing_mm: spacing_mm,
                        col_spacing_mm: spacing_mm,
                        slice_thickness_mm,
                    },
                    cli.window_center,
                    cli.window_width,
                    output_path,
                )?;
            } else {
                for (index, depth_mm) in depths.iter().enumerate() {
                    let path = frame_output_path(output_path, index + 1)?;
                    let values = reformat_plane_values(&volume, &plane_params_at(*depth_mm))
                        .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?;
                    write_reformatted_dicom_slice(
                        &source_object,
                        &values,
                        output_height,
                        output_width,
                        &series_instance_uid,
                        (index + 1) as u32,
                        &SliceGeometry {
                            position: first_voxel_center_at(*depth_mm),
                            row_dir,
                            col_dir,
                            row_spacing_mm: spacing_mm,
                            col_spacing_mm: spacing_mm,
                            slice_thickness_mm,
                        },
                        cli.window_center,
                        cli.window_width,
                        &path,
                    )?;
                }
            }
        }
        MprOutputKind::Volume(volume_format) => {
            let mut samples = Vec::with_capacity(output_width as usize * output_height as usize * depths.len());
            for depth_mm in &depths {
                let values = reformat_plane_values(&volume, &plane_params_at(*depth_mm))
                    .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?;
                samples.extend(values);
            }
            let step_mm = if depths.len() > 1 { depths[1] - depths[0] } else { default_depth_step };
            let geometry = VolumeGeometry {
                row_dir,
                col_dir,
                normal_dir: normal,
                col_spacing_mm: spacing_mm,
                row_spacing_mm: spacing_mm,
                step_mm,
                origin: first_voxel_center_at(depths[0]),
            };
            let dims = (output_width, output_height, depths.len() as u32);
            match volume_format {
                VolumeFormat::Nifti => write_nifti(&samples, dims, &geometry, output_path, false)
                    .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?,
                VolumeFormat::NiftiGz => write_nifti(&samples, dims, &geometry, output_path, true)
                    .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?,
                VolumeFormat::Nrrd => write_nrrd(&samples, dims, &geometry, output_path)
                    .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))?,
            }
        }
    }

    verbose_log(cli, format!("Wrote {} reformatted slice(s)", depths.len()));
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum VolumeFormat {
    Nifti,
    NiftiGz,
    Nrrd,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum MprOutputKind {
    Volume(VolumeFormat),
    DicomSeries,
    Rendered(RenderFormat),
}

/// Determines what kind of output `--mpr` should produce from `output_path`'s extension -
/// checked BEFORE `resolve_render_format` (whose "unknown extension" error doesn't know about
/// `.nii`/`.nii.gz`/`.nrrd`/`.dcm`).
fn resolve_mpr_output_kind(cli: &Cli, output_path: &Path) -> Result<MprOutputKind, Box<dyn std::error::Error>> {
    let file_name = output_path.file_name().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
    if file_name.ends_with(".nii.gz") {
        return Ok(MprOutputKind::Volume(VolumeFormat::NiftiGz));
    }
    if file_name.ends_with(".nii") {
        return Ok(MprOutputKind::Volume(VolumeFormat::Nifti));
    }
    if file_name.ends_with(".nrrd") {
        return Ok(MprOutputKind::Volume(VolumeFormat::Nrrd));
    }
    if file_name.ends_with(".dcm") || file_name.ends_with(".dicom") {
        return Ok(MprOutputKind::DicomSeries);
    }
    Ok(MprOutputKind::Rendered(resolve_render_format(cli, output_path)?))
}

fn parse_triplet(value: &str, flag_name: &str) -> Result<(f64, f64, f64), io::Error> {
    let parts: Vec<&str> = value.split(',').collect();
    if parts.len() != 3 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("{flag_name} expects three comma-separated numbers (e.g. 1.0,2.0,3.0)"),
        ));
    }
    let parse_one = |raw: &str| -> Result<f64, io::Error> {
        raw.trim()
            .parse::<f64>()
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, format!("{flag_name} contains an invalid number: '{raw}'")))
    };
    Ok((parse_one(parts[0])?, parse_one(parts[1])?, parse_one(parts[2])?))
}

/// A parsed `--mpr-depth` value - see that flag's own help text for the exact syntax. `Single`
/// preserves the original "one offset, one plane" behavior exactly; `Range`/`All` describe a
/// STACK of slices (for multi-file .png/.jpg/.dcm output, or a single whole-volume .nii/.nrrd).
#[derive(Clone, Copy, Debug, PartialEq)]
enum DepthSpec {
    Single(f64),
    Range { start: f64, end: f64, step: Option<f64> },
    All { step: Option<f64> },
}

fn invalid_mpr_depth_error(value: &str) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!(
            "invalid --mpr-depth value '{value}': expected a single MM offset, START:END, START:END:STEP, 'all', or 'all:STEP'"
        ),
    )
}

fn parse_depth_spec(value: &str) -> Result<DepthSpec, io::Error> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower == "all" {
        return Ok(DepthSpec::All { step: None });
    }
    if let Some(rest) = lower.strip_prefix("all:") {
        let step: f64 = rest.trim().parse().map_err(|_| invalid_mpr_depth_error(value))?;
        if !(step > 0.0) {
            return Err(invalid_mpr_depth_error(value));
        }
        return Ok(DepthSpec::All { step: Some(step) });
    }

    let parts: Vec<&str> = trimmed.split(':').collect();
    match parts.as_slice() {
        [single] => single.trim().parse().map(DepthSpec::Single).map_err(|_| invalid_mpr_depth_error(value)),
        [start, end] => {
            let start: f64 = start.trim().parse().map_err(|_| invalid_mpr_depth_error(value))?;
            let end: f64 = end.trim().parse().map_err(|_| invalid_mpr_depth_error(value))?;
            Ok(DepthSpec::Range { start, end, step: None })
        }
        [start, end, step] => {
            let start: f64 = start.trim().parse().map_err(|_| invalid_mpr_depth_error(value))?;
            let end: f64 = end.trim().parse().map_err(|_| invalid_mpr_depth_error(value))?;
            let step: f64 = step.trim().parse().map_err(|_| invalid_mpr_depth_error(value))?;
            if !(step > 0.0) {
                return Err(invalid_mpr_depth_error(value));
            }
            Ok(DepthSpec::Range { start, end, step: Some(step) })
        }
        _ => Err(invalid_mpr_depth_error(value)),
    }
}

/// Every depth offset from `lo` to `hi` (inclusive of `lo`; `hi` is included only if it lands
/// exactly on a step) at `step` millimeters apart. Always yields at least one depth.
fn depth_steps(start: f64, end: f64, step: f64) -> Vec<f64> {
    let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
    let span = hi - lo;
    let count = ((span / step).floor() as i64 + 1).max(1) as usize;
    (0..count).map(|index| lo + step * index as f64).collect()
}

/// The volume's own full extent along the reformat plane's normal, as a depth RANGE relative to
/// `base_origin` (i.e. directly usable as `depth_steps`' start/end) - computed by projecting all
/// 8 corners of the volume's physical bounding box onto the normal, which is correct even for an
/// oblique plane whose normal differs from the volume's own `slice_normal`.
fn volume_depth_extent(volume: &Volume, row_dir: [f64; 3], col_dir: [f64; 3], base_origin: [f64; 3]) -> (f64, f64) {
    let normal_raw = [
        row_dir[1] * col_dir[2] - row_dir[2] * col_dir[1],
        row_dir[2] * col_dir[0] - row_dir[0] * col_dir[2],
        row_dir[0] * col_dir[1] - row_dir[1] * col_dir[0],
    ];
    let norm_len = (normal_raw[0].powi(2) + normal_raw[1].powi(2) + normal_raw[2].powi(2)).sqrt().max(1e-9);
    let normal = [normal_raw[0] / norm_len, normal_raw[1] / norm_len, normal_raw[2] / norm_len];

    let last_col = volume.cols.saturating_sub(1) as f64 * volume.col_spacing_mm;
    let last_row = volume.rows.saturating_sub(1) as f64 * volume.row_spacing_mm;
    let z_first = volume.slice_zs.first().copied().unwrap_or(0.0);
    let z_last = volume.slice_zs.last().copied().unwrap_or(0.0);
    let base_proj = base_origin[0] * normal[0] + base_origin[1] * normal[1] + base_origin[2] * normal[2];

    let mut min_rel = f64::INFINITY;
    let mut max_rel = f64::NEG_INFINITY;
    for &c in &[0.0, last_col] {
        for &r in &[0.0, last_row] {
            for &z in &[z_first, z_last] {
                let world = [
                    volume.origin[0] + volume.row_vector[0] * c + volume.col_vector[0] * r + volume.slice_normal[0] * (z - z_first),
                    volume.origin[1] + volume.row_vector[1] * c + volume.col_vector[1] * r + volume.slice_normal[1] * (z - z_first),
                    volume.origin[2] + volume.row_vector[2] * c + volume.col_vector[2] * r + volume.slice_normal[2] * (z - z_first),
                ];
                let proj = world[0] * normal[0] + world[1] * normal[1] + world[2] * normal[2] - base_proj;
                min_rel = min_rel.min(proj);
                max_rel = max_rel.max(proj);
            }
        }
    }
    (min_rel, max_rel)
}

/// Resolves a `DepthSpec` into the concrete list of depth offsets `run_mpr` reformats - always at
/// least one. `default_step` is used whenever a `Range`/`All` doesn't specify its own `:STEP`.
fn resolve_depths(spec: DepthSpec, volume: &Volume, row_dir: [f64; 3], col_dir: [f64; 3], base_origin: [f64; 3], default_step: f64) -> Vec<f64> {
    match spec {
        DepthSpec::Single(value) => vec![value],
        DepthSpec::Range { start, end, step } => depth_steps(start, end, step.unwrap_or(default_step)),
        DepthSpec::All { step } => {
            let (start, end) = volume_depth_extent(volume, row_dir, col_dir, base_origin);
            depth_steps(start, end, step.unwrap_or(default_step))
        }
    }
}

fn run_json_to_dicom(cli: &Cli, input_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let _scope = perf::scope("cli.run_json_to_dicom");
    validate_no_render_or_redaction_flags(cli)?;

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

    let bulk_data_source = cli.bulk_data_source.as_deref().map(fs::read).transpose()?;

    let mut object = {
        let _read_scope = perf::scope("cli.json_to_dicom.read_dicom_json_with_options");
        read_dicom_json_with_options(
            json,
            DicomJsonReadOptions {
                format: match cli.format {
                    JsonFormat::Flat => DicomJsonFormat::Flat,
                    JsonFormat::Standard => DicomJsonFormat::Standard,
                },
                bulk_data_source: bulk_data_source.as_deref(),
            },
        )?
    };
    apply_attribute_overrides(cli, &mut object)?;

    verbose_log(
        cli,
        format!(
            "Converting JSON to DICOM (format={:?}, output={})",
            cli.format,
            output_path.display()
        ),
    );
    write_dicom_file(&mut object, output_path)?;
    Ok(())
}

fn apply_attribute_overrides(
    cli: &Cli,
    object: &mut dicom_object::DefaultDicomObject,
) -> Result<(), Box<dyn std::error::Error>> {
    for assignment in &cli.set {
        let (tag, vr, value) = parse_attribute_override(assignment)?;
        set_attribute(object, tag, vr, value)?;
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

    for key in &cli.remove {
        let tag = parse_tag_key(key)?;
        let was_present = remove_attribute(object, tag);
        verbose_log(
            cli,
            format!(
                "Remove {} ({:04X},{:04X}){}",
                keyword_for_tag(tag),
                tag.group(),
                tag.element(),
                if was_present { "" } else { " (not present)" },
            ),
        );
    }

    Ok(())
}

fn parse_redact_box(s: &str) -> Result<BoundingBox, io::Error> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --redact-box value '{s}'; expected X,Y,W,H where X/Y are integers (negative allowed for right/bottom anchoring) and W/H are non-negative integers or percentages like 25%"
            ),
        ));
    }
    let x = parse_redact_coordinate(parts[0].trim(), s, "X")?;
    let y = parse_redact_coordinate(parts[1].trim(), s, "Y")?;
    let width = parse_redact_extent(parts[2].trim(), s, "W")?;
    let height = parse_redact_extent(parts[3].trim(), s, "H")?;

    Ok(BoundingBox {
        x,
        y,
        width,
        height,
    })
}

fn parse_redact_extent(value: &str, original: &str, axis: &str) -> Result<BoxLength, io::Error> {
    if let Some(percent) = value.strip_suffix('%') {
        let parsed = percent.trim().parse::<f64>().map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "invalid --redact-box value '{original}'; {axis} must be a non-negative integer or percentage like 25%"
                ),
            )
        })?;

        if !parsed.is_finite() || parsed < 0.0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "invalid --redact-box value '{original}'; {axis} percentage must be non-negative"
                ),
            ));
        }

        return Ok(BoxLength::Percent(parsed));
    }

    let parsed = value.parse::<u32>().map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --redact-box value '{original}'; {axis} must be a non-negative integer or percentage like 25%"
            ),
        )
    })?;
    Ok(BoxLength::Pixels(parsed))
}

fn parse_redact_coordinate(value: &str, original: &str, axis: &str) -> Result<i32, io::Error> {
    let parsed = value.parse::<i32>().map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --redact-box value '{original}'; {axis} must be an integer (negative allowed)"
            ),
        )
    })?;

    if parsed == 0 && value.starts_with('-') {
        return Ok(NEGATIVE_ZERO_ANCHOR);
    }

    Ok(parsed)
}

fn parse_redact_color(s: &str) -> Result<[u8; 3], io::Error> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid --redact-color value '{s}'; hex color must be #RRGGBB"),
            ));
        }
        let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid --redact-color value '{s}'; not a valid hex color"),
            )
        })?;
        let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid --redact-color value '{s}'; not a valid hex color"),
            )
        })?;
        let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid --redact-color value '{s}'; not a valid hex color"),
            )
        })?;
        return Ok([r, g, b]);
    }

    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --redact-color value '{s}'; expected R,G,B (three comma-separated 0-255 integers) or #RRGGBB"
            ),
        ));
    }
    let mut values = [0u8; 3];
    for (i, part) in parts.iter().enumerate() {
        values[i] = part.trim().parse::<u8>().map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid --redact-color value '{s}'; each component must be 0-255"),
            )
        })?;
    }
    Ok(values)
}

fn keyword_for_tag(tag: Tag) -> String {
    StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.alias().to_owned())
        .unwrap_or_else(|| format!("({:04X},{:04X})", tag.group(), tag.element()))
}

fn infer_direction(
    cli: &Cli,
    input: &Path,
    input_bytes: &[u8],
) -> Result<Direction, Box<dyn std::error::Error>> {
    let input_kind = detect_input_kind(input, input_bytes, cli.input_type)?;

    if cli.overwrite && cli.output.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--overwrite cannot be combined with an explicit output path",
        )
        .into());
    }

    match (&cli.output, input_kind) {
        (Some(output), FileKind::Dicom) => match detect_output_kind(output, cli.output_type) {
            Some(FileKind::Json) => Ok(Direction::DicomToJson),
            Some(FileKind::Dicom) => Ok(Direction::DicomToDicom),
            Some(FileKind::Render) => Ok(Direction::DicomToRender),
            None => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "could not determine output type; use .json, .dcm/.dicom, or a render extension (.jpg/.jpeg/.png/.raw/.mp4/.m4v/.mpeg4/.mov), or use --output-type",
            )
            .into()),
        },
        (Some(output), FileKind::Json) => match detect_output_kind(output, cli.output_type) {
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
                "could not determine output type; use .json, .dcm/.dicom, or a render extension (.jpg/.jpeg/.png/.raw/.mp4/.m4v/.mpeg4/.mov), or use --output-type",
            )
            .into()),
        },
        (None, FileKind::Dicom) => {
            if cli.overwrite {
                Ok(Direction::DicomToDicom)
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
    explicit_type: Option<InputType>,
) -> Result<FileKind, Box<dyn std::error::Error>> {
    if let Some(input_type) = explicit_type {
        return Ok(match input_type {
            InputType::Dicom => FileKind::Dicom,
            InputType::Json => FileKind::Json,
        });
    }

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
        "could not determine input type; use a .json, .dcm, or .dicom extension, or use --input-type",
    )
    .into())
}

fn detect_output_kind(path: &Path, explicit_type: Option<OutputType>) -> Option<FileKind> {
    if let Some(output_type) = explicit_type {
        return Some(match output_type {
            OutputType::Dicom => FileKind::Dicom,
            OutputType::Json => FileKind::Json,
            OutputType::Raw | OutputType::Png | OutputType::Jpeg | OutputType::Mpeg4 | OutputType::Texture => FileKind::Render,
        });
    }

    detect_kind_from_extension(path)
}

fn detect_kind_from_extension(path: &Path) -> Option<FileKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();

    match extension.as_str() {
        "json" => Some(FileKind::Json),
        "dcm" | "dicom" => Some(FileKind::Dicom),
        "jpg" | "jpeg" | "png" | "raw" | "mp4" | "m4v" | "mpeg4" | "mov" | "gputex" => Some(FileKind::Render),
        _ => None,
    }
}

fn output_type_to_render_format(output_type: OutputType) -> Option<RenderFormat> {
    match output_type {
        OutputType::Raw => Some(RenderFormat::Raw),
        OutputType::Png => Some(RenderFormat::Png),
        OutputType::Jpeg => Some(RenderFormat::Jpeg),
        OutputType::Mpeg4 => Some(RenderFormat::Mpeg4),
        OutputType::Texture => Some(RenderFormat::Texture),
        _ => None,
    }
}

fn validate_no_render_or_redaction_flags(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let is_render_output = cli.output_type.map_or(false, |ot| output_type_to_render_format(ot).is_some());
    
    if is_render_output
        || cli.render_frame != 0
        || cli.no_modality_lut
        || cli.no_voi_lut
        || cli.no_icc_profile
        || cli.window_center.is_some()
        || cli.window_width.is_some()
        || cli.jpeg_quality != 90
        || cli.render_all_frames
        || cli.render_fps.is_some()
        || cli.output_width.is_some()
        || cli.output_height.is_some()
        || cli.scale_max_size.is_some()
        || !cli.redact_box.is_empty()
        || cli.redact_color.is_some()
        || cli.pad
        || cli.pad_color.is_some()
        || cli.no_overlays
        || cli.overlay_index.is_some()
        || cli.overlay_color.is_some()
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "render options are only valid when converting DICOM to render format (raw/png/jpeg/mpeg4)",
        )
        .into());
    }

    Ok(())
}

fn validate_check_dicom_flags(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.output.is_some()
        || cli.overwrite
        || cli.input_type.is_some()
        || cli.output_type.is_some()
        || cli.transfer_syntax.is_some()
        || !cli.set.is_empty()
        || !cli.remove.is_empty()
        || cli.remove_private_tags
        || cli.format != JsonFormat::Flat
        || cli.keys != KeyFormat::Name
        || cli.bulk_data != BulkDataMode::Uri
        || cli.bulk_data_source.is_some()
        || cli.render_frame != 0
        || cli.render_all_frames
        || cli.render_fps.is_some()
        || cli.no_modality_lut
        || cli.no_voi_lut
        || cli.no_icc_profile
        || cli.window_center.is_some()
        || cli.window_width.is_some()
        || cli.jpeg_quality != 90
        || cli.output_width.is_some()
        || cli.output_height.is_some()
        || cli.scale_max_size.is_some()
        || !cli.redact_box.is_empty()
        || cli.redact_color.is_some()
        || cli.pad
        || cli.pad_color.is_some()
        || cli.no_overlays
        || cli.overlay_index.is_some()
        || cli.overlay_color.is_some()
        || !cli.filter.is_empty()
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--check-dicom only accepts INPUT (or --stdin-paths) and optional --verbose",
        )
        .into());
    }

    Ok(())
}

fn validate_histogram_flags(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.histogram_min.is_some() != cli.histogram_max.is_some() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--histogram-min and --histogram-max must be given together",
        )
        .into());
    }

    Ok(())
}

fn run_histogram(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let input_path = cli.input.as_ref().ok_or_else(|| {
        io::Error::new(ErrorKind::InvalidInput, "an input path is required for --histogram")
    })?;

    let object = read_dicom_file(input_path)?;
    let options = HistogramOptions {
        bin_count: cli.histogram_bins,
        min_value: cli.histogram_min,
        max_value: cli.histogram_max,
    };

    let histograms = match cli.histogram_frame {
        Some(frame_index) => vec![compute_frame_histogram(&object, frame_index, &options)?],
        None => compute_instance_histograms(&object, &options)?,
    };

    let json_frames: Vec<JsonValue> = histograms
        .iter()
        .map(|histogram| {
            serde_json::json!({
                "frameIndex": histogram.frame_index,
                "binCount": histogram.bin_count,
                "rangeMin": histogram.range_min,
                "rangeMax": histogram.range_max,
                "binWidth": histogram.bin_width,
                "counts": histogram.counts,
                "pixelCount": histogram.pixel_count,
                "minValue": histogram.min_value,
                "maxValue": histogram.max_value,
                "mean": histogram.mean,
                "stdDev": histogram.std_dev,
            })
        })
        .collect();

    let output_json = serde_json::json!({ "frames": json_frames });
    let text = serde_json::to_string_pretty(&output_json)?;

    match cli.output.as_ref() {
        Some(path) => fs::write(path, text)?,
        None => println!("{text}"),
    }

    Ok(())
}

fn validate_non_dicom_to_dicom_render_flags(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let is_render_output = cli.output_type.map_or(false, |ot| output_type_to_render_format(ot).is_some());
    
    if is_render_output
        || cli.render_frame != 0
        || cli.no_modality_lut
        || cli.no_voi_lut
        || cli.no_icc_profile
        || cli.window_center.is_some()
        || cli.window_width.is_some()
        || cli.jpeg_quality != 90
        || cli.render_all_frames
        || cli.render_fps.is_some()
        || cli.output_width.is_some()
        || cli.output_height.is_some()
        || cli.scale_max_size.is_some()
        || cli.pad
        || cli.pad_color.is_some()
        || cli.no_overlays
        || cli.overlay_index.is_some()
        || cli.overlay_color.is_some()
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "render options are only valid when converting DICOM to .jpg/.jpeg/.png/.raw/.mp4",
        )
        .into());
    }

    Ok(())
}

fn ensure_supported_redaction_target_transfer_syntax(uid: &str) -> Result<(), io::Error> {
    let support = list_transfer_syntax_support();
    let Some(entry) = support.iter().find(|entry| entry.uid == uid) else {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "we do not support transcoding to {uid} transfer syntax. Run --list-transfer-syntaxes to see supported transfer syntaxes"
            ),
        ));
    };

    if !entry.can_write_dataset {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "we do not support transcoding to {uid} transfer syntax. Run --list-transfer-syntaxes to see supported transfer syntaxes"
            ),
        ));
    }

    if entry.encapsulated_pixel_data && !entry.can_encode_pixel_data {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "we do not support transcoding to {uid} transfer syntax because PIXEL_ENCODE is not supported. Run --list-transfer-syntaxes to see supported transfer syntaxes"
            ),
        ));
    }

    Ok(())
}

fn resolve_render_format(
    cli: &Cli,
    output_path: &Path,
) -> Result<RenderFormat, Box<dyn std::error::Error>> {
    if let Some(output_type) = cli.output_type {
        if let Some(format) = output_type_to_render_format(output_type) {
            return Ok(format);
        }
    }

    let extension = output_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "render output requires --output-type (raw/png/jpeg/mpeg4) when output extension is missing",
            )
        })?;

    match extension.as_str() {
        "raw" => Ok(RenderFormat::Raw),
        "png" => Ok(RenderFormat::Png),
        "jpg" | "jpeg" => Ok(RenderFormat::Jpeg),
        "mp4" | "m4v" | "mpeg4" | "mov" => Ok(RenderFormat::Mpeg4),
        "gputex" => Ok(RenderFormat::Texture),
        _ => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "render output extension must be .raw, .png, .jpg/.jpeg, .mp4/.m4v/.mpeg4/.mov, or .gputex, or use --output-type (raw/png/jpeg/mpeg4/texture)",
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
        RenderFormat::Texture => {
            unreachable!("RenderFormat::Texture is handled directly in run_dicom_to_render_with_object/run_mpr, before reaching to_render_output_format")
        }
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
        RenderFormat::Texture => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "texture output is handled separately and does not support --render-all-frames",
        )
        .into()),
    }
}

fn resolve_texture_compression(cli: &Cli) -> TextureCompression {
    match cli.texture_compression {
        Some(TextureCompressionArg::None) => TextureCompression::None,
        Some(TextureCompressionArg::Gzip) | None => TextureCompression::Gzip,
    }
}

/// Writes a `PackedTexture` as `output_path` (the raw/gzip payload bytes) plus
/// `output_path`-with-`.json`-appended (the `TextureMeta` sidecar) - the standalone,
/// server-independent contract a texture-format export is meant to expose (see the CLI's
/// "Texture Export" flags).
fn write_packed_texture(output_path: &Path, packed: &PackedTexture) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(output_path, &packed.payload)?;
    let mut sidecar_name = output_path.as_os_str().to_owned();
    sidecar_name.push(".json");
    let json_text = serde_json::to_string_pretty(&packed.meta.to_json())?;
    fs::write(PathBuf::from(sidecar_name), json_text)?;
    Ok(())
}

fn frame_output_path(
    base: &Path,
    frame_number: usize,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
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

fn first_numeric_tag(
    object: &dicom_object::DefaultDicomObject,
    tag: dicom_core::Tag,
) -> Option<f64> {
    object
        .get(tag)
        .and_then(|element| element.to_str().ok())
        .and_then(|text| {
            text.split('\\')
                .next()
                .and_then(|part| part.trim().parse::<f64>().ok())
        })
}

fn numeric_values_tag(
    object: &dicom_object::DefaultDicomObject,
    tag: dicom_core::Tag,
) -> Option<Vec<f64>> {
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
        apply_filter_to_object, build_volume, canonical_view_basis, depth_steps, detect_output_kind,
        frame_output_path, infer_direction, parse_attribute_override, parse_depth_spec,
        parse_filter_requests, parse_redact_box, resolve_depths, resolve_mpr_output_kind,
        run_dicom_to_json_with_object, run_dicom_to_render_with_object, run_mpr, resolve_render_format,
        Cli, DepthSpec, Direction, FileKind, MprOutputKind, OutputType, RenderFormat, TextureCompressionArg,
        VolumeFormat,
    };
    use clap::{CommandFactory, FromArgMatches};
    use dicom_dictionary_std::tags;
    use dcmnorm::dicom_io::{next_tag, read_dicom_file, write_dicom_file};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn fixture_path(name: &str) -> PathBuf {
        repo_root().join("test").join("files").join(name)
    }

    fn temp_output_path(stem: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("dcmnorm-{stem}-{nanos}.json"))
    }

    fn base_cli() -> Cli {
        Cli {
            paths: Vec::new(),
            input: None,
            output: None,
            input_type: None,
            output_type: None,
            jpeg2000_codec: super::Jpeg2000Codec::Auto,
            stdin_paths: false,
            filter: Vec::new(),
            overwrite: false,
            check_dicom: false,
            format: super::JsonFormat::Flat,
            keys: super::KeyFormat::Name,
            bulk_data: super::BulkDataMode::Uri,
            bulk_data_source: None,
            transfer_syntax: None,
            set: Vec::new(),
            remove: Vec::new(),
            render_frame: 0,
            no_modality_lut: false,
            no_voi_lut: false,
            no_icc_profile: false,
            window_center: None,
            window_width: None,
            jpeg_quality: 90,
            render_all_frames: false,
            render_fps: None,
            output_width: None,
            output_height: None,
            scale_max_size: None,
            redact_box: Vec::new(),
            redact_color: None,
            pad: false,
            pad_color: None,
            no_overlays: false,
            overlay_index: None,
            overlay_color: None,
            list_transfer_syntaxes: false,
            verbose: false,
            remove_private_tags: false,
            mpr: None,
            mpr_origin: None,
            mpr_depth: None,
            mpr_spacing: None,
            mpr_thickness: None,
            mpr_projection: None,
            texture_max_dim: None,
            texture_compression: None,
        }
    }

    #[test]
    fn parses_check_dicom_flag() {
        let matches = Cli::command()
            .try_get_matches_from(["dcmnorm", "--check-dicom", "in.dcm"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert!(cli.check_dicom);
        assert_eq!(cli.input, Some(PathBuf::from("in.dcm")));
    }

    #[test]
    fn parses_overlay_rendering_flags() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dcmnorm",
                "--overlay-index",
                "1",
                "--overlay-color",
                "255,0,0",
                "in.dcm",
                "out.png",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert!(!cli.no_overlays);
        assert_eq!(cli.overlay_index, Some(1));
        assert_eq!(cli.overlay_color, Some("255,0,0".to_string()));
    }

    #[test]
    fn parses_no_overlays_flag() {
        let matches = Cli::command()
            .try_get_matches_from(["dcmnorm", "--no-overlays", "in.dcm", "out.png"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert!(cli.no_overlays);
        assert_eq!(cli.overlay_index, None);
    }

    #[test]
    fn no_render_or_redaction_flags_rejects_overlay_flags_outside_render_output() {
        let mut cli = base_cli();
        cli.no_overlays = true;

        let error = super::validate_no_render_or_redaction_flags(&cli).unwrap_err();
        assert!(error.to_string().contains("render options are only valid"));
    }

    #[test]
    fn non_dicom_to_dicom_render_flags_rejects_overlay_index_outside_render_output() {
        let mut cli = base_cli();
        cli.overlay_index = Some(2);

        let error = super::validate_non_dicom_to_dicom_render_flags(&cli).unwrap_err();
        assert!(error.to_string().contains("render options are only valid"));
    }

    #[test]
    fn parses_filter_flag_with_keyword() {
        let matches = Cli::command()
            .try_get_matches_from(["dcmnorm", "--filter", "StudyInstanceUID", "in.dcm"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert_eq!(cli.filter, vec!["StudyInstanceUID".to_string()]);
    }

    #[test]
    fn parses_filter_flag_with_comma_separated_values() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dcmnorm",
                "--filter",
                "StudyInstanceUID,PatientID",
                "in.dcm",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert_eq!(
            cli.filter,
            vec!["StudyInstanceUID".to_string(), "PatientID".to_string()]
        );
    }

    #[test]
    fn computes_next_tag_for_element_and_group_boundaries() {
        assert_eq!(next_tag(Tag(0x0010, 0x0010)), Some(Tag(0x0010, 0x0011)));
        assert_eq!(next_tag(Tag(0x0010, 0xFFFF)), Some(Tag(0x0011, 0x0000)));
        assert_eq!(next_tag(Tag(0xFFFF, 0xFFFF)), None);
    }

    #[test]
    fn parses_filter_requests_with_keyword_and_tag_expression() {
        let requests = parse_filter_requests(&[
            "StudyInstanceUID".to_string(),
            "(0008,0060)".to_string(),
        ])
        .unwrap();

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tag, tags::STUDY_INSTANCE_UID);
        assert_eq!(requests[1].tag, tags::MODALITY);
    }

    #[test]
    fn apply_filter_keeps_only_requested_tags() {
        let requests = parse_filter_requests(&["StudyInstanceUID".to_string()]).unwrap();
        let mut object = read_dicom_file(fixture_path("dx.dcm")).unwrap();

        apply_filter_to_object(&mut object, &requests);

        assert!(object.get(tags::STUDY_INSTANCE_UID).is_some());
        assert!(object.get(tags::MODALITY).is_none());
        assert!(object.get(tags::PIXEL_DATA).is_none());
    }

    #[test]
    fn filtered_dicom_to_json_writes_only_filtered_attributes() {
        let requests = parse_filter_requests(&["StudyInstanceUID".to_string()]).unwrap();
        let input_path = fixture_path("dx.dcm");
        let mut object = read_dicom_file(&input_path).unwrap();
        apply_filter_to_object(&mut object, &requests);

        let mut cli = base_cli();
        let output_path = temp_output_path("filtered-dicom-to-json");
        cli.output = Some(output_path.clone());
        cli.filter = vec!["StudyInstanceUID".to_string()];

        run_dicom_to_json_with_object(&cli, Path::new(&input_path), None, object).unwrap();

        let json = fs::read_to_string(&output_path).unwrap();
        let _ = fs::remove_file(&output_path);
        assert!(json.contains("\"StudyInstanceUID\""));
        assert!(!json.contains("\"Modality\""));
        assert!(!json.contains("\"FileMetaInformationVersion\""));
        assert!(!json.contains("\"TransferSyntaxUID\""));
    }

    #[test]
    fn run_dicom_to_render_with_object_writes_a_valid_single_frame_texture_export() {
        let object = read_dicom_file(fixture_path("ct.dcm")).unwrap();

        let mut cli = base_cli();
        let output_path = temp_output_path("frame-texture").with_extension("gputex");
        cli.output = Some(output_path.clone());
        cli.window_center = Some(40.0);
        cli.window_width = Some(400.0);

        run_dicom_to_render_with_object(&cli, object).unwrap();

        let sidecar_path = {
            let mut name = output_path.as_os_str().to_owned();
            name.push(".json");
            PathBuf::from(name)
        };
        let meta: serde_json::Value = serde_json::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(meta["contentKind"], "image2d");
        assert_eq!(meta["depth"], 1);
        assert_eq!(meta["width"], 512);
        assert_eq!(meta["height"], 512);
        assert_eq!(meta["defaultWindowCenter"], 40.0);
        assert_eq!(meta["defaultWindowWidth"], 400.0);
        let payload_bytes_raw = meta["payloadBytesRaw"].as_u64().unwrap();
        assert_eq!(payload_bytes_raw, 512 * 512 * 2);

        fs::remove_file(&output_path).ok();
        fs::remove_file(&sidecar_path).ok();
    }

    #[test]
    fn run_dicom_to_render_with_object_rejects_output_width_with_texture_output() {
        let object = read_dicom_file(fixture_path("ct.dcm")).unwrap();

        let mut cli = base_cli();
        let output_path = temp_output_path("frame-texture-conflict").with_extension("gputex");
        cli.output = Some(output_path.clone());
        cli.output_width = Some(256);

        assert!(run_dicom_to_render_with_object(&cli, object).is_err());
        assert!(!output_path.exists());
    }

    #[test]
    fn parses_mpr_flag_with_a_canonical_view_value() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dcmnorm", "-I", "--mpr", "coronal", "--mpr-origin", "1,2,3", "--mpr-spacing", "0.5", "out.png",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert!(cli.stdin_paths);
        assert_eq!(cli.mpr.as_deref(), Some("coronal"));
        assert_eq!(cli.mpr_origin.as_deref(), Some("1,2,3"));
        assert_eq!(cli.mpr_spacing, Some(0.5));
        assert!(cli.mpr_requested());
        // With no OUTPUT positional given, the lone positional lands in `input` -
        // `run_mpr` is responsible for accepting it as the output path (see its own doc comment).
        assert_eq!(cli.input, Some(PathBuf::from("out.png")));
        assert_eq!(cli.output, None);
    }

    #[test]
    fn parses_mpr_flag_with_a_rotation_triplet_value() {
        let matches = Cli::command()
            .try_get_matches_from(["dcmnorm", "--mpr", "15,30,0", "a.dcm", "b.dcm", "out.png"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert_eq!(cli.mpr.as_deref(), Some("15,30,0"));
        assert!(cli.mpr_requested());
    }

    #[test]
    fn mpr_origin_without_mpr_is_not_requested() {
        // --mpr-origin/--mpr-spacing alone don't imply MPR mode (unlike --mpr itself, they carry
        // no "which plane" information) - run() explicitly rejects this combination rather than
        // silently ignoring them; mpr_requested() reflects that they don't self-trigger MPR.
        let cli = Cli { mpr_origin: Some("1,2,3".to_string()), ..base_cli() };
        assert!(!cli.mpr_requested());
    }

    #[test]
    fn parses_mpr_depth_thickness_and_projection_flags() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dcmnorm", "--mpr", "coronal", "--mpr-depth", "-15.5", "--mpr-thickness", "20",
                "--mpr-projection", "minip", "series_dir/a.dcm", "series_dir/b.dcm", "out.png",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();

        assert_eq!(cli.mpr_depth.as_deref(), Some("-15.5"));
        assert_eq!(cli.mpr_thickness, Some(20.0));
        assert_eq!(cli.mpr_projection.as_deref(), Some("minip"));
    }

    #[test]
    fn run_mpr_depth_offsets_the_plane_along_its_own_normal() {
        // Each slice has distinct absolute values (see the helper's own doc comment) and a fixed
        // (not auto-normalized) window, so a depth-driven slice change is actually visible in the
        // output bytes rather than hidden by robust min-max normalization or fixture slices that
        // all share the same underlying pixel content.
        let paths = write_synthetic_ct_series_with_varying_intercept_for_cli(6, 1.0);

        let mut centered_cli = base_cli();
        centered_cli.mpr = Some("axial".to_string());
        centered_cli.output_type = Some(OutputType::Raw);
        centered_cli.window_center = Some(500.0);
        centered_cli.window_width = Some(4000.0);
        let centered_output = temp_output_path("mpr-depth-centered").with_extension("raw");
        run_mpr(&centered_cli, &paths, &centered_output).unwrap();
        let centered_bytes = fs::read(&centered_output).unwrap();

        let mut deep_cli = base_cli();
        deep_cli.mpr = Some("axial".to_string());
        deep_cli.mpr_depth = Some("2.0".to_string());
        deep_cli.output_type = Some(OutputType::Raw);
        deep_cli.window_center = Some(500.0);
        deep_cli.window_width = Some(4000.0);
        let deep_output = temp_output_path("mpr-depth-offset").with_extension("raw");
        run_mpr(&deep_cli, &paths, &deep_output).unwrap();
        let deep_bytes = fs::read(&deep_output).unwrap();

        assert_ne!(centered_bytes, deep_bytes, "--mpr-depth should move the reformatted plane to a different slice");

        fs::remove_file(&centered_output).ok();
        fs::remove_file(&deep_output).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_writes_a_thick_mip_slab() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-thick-slab").with_extension("png");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        cli.mpr_thickness = Some(4.0);
        cli.mpr_projection = Some("mip".to_string());

        run_mpr(&cli, &paths, &output_path).unwrap();

        let metadata = fs::metadata(&output_path).unwrap();
        assert!(metadata.len() > 0);

        fs::remove_file(&output_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn parse_depth_spec_parses_every_syntax_variant() {
        assert_eq!(parse_depth_spec("5").unwrap(), DepthSpec::Single(5.0));
        assert_eq!(parse_depth_spec("-15.5").unwrap(), DepthSpec::Single(-15.5));
        assert_eq!(parse_depth_spec("-10:10").unwrap(), DepthSpec::Range { start: -10.0, end: 10.0, step: None });
        assert_eq!(
            parse_depth_spec("-10:10:2.5").unwrap(),
            DepthSpec::Range { start: -10.0, end: 10.0, step: Some(2.5) }
        );
        assert_eq!(parse_depth_spec("all").unwrap(), DepthSpec::All { step: None });
        assert_eq!(parse_depth_spec("ALL").unwrap(), DepthSpec::All { step: None });
        assert_eq!(parse_depth_spec("all:3").unwrap(), DepthSpec::All { step: Some(3.0) });
    }

    #[test]
    fn parse_depth_spec_rejects_garbage_and_non_positive_steps() {
        assert!(parse_depth_spec("not-a-number").is_err());
        assert!(parse_depth_spec("1:2:3:4").is_err());
        assert!(parse_depth_spec("1:2:0").is_err());
        assert!(parse_depth_spec("1:2:-1").is_err());
        assert!(parse_depth_spec("all:0").is_err());
        assert!(parse_depth_spec("all:-5").is_err());
    }

    #[test]
    fn depth_steps_always_includes_the_start_and_yields_at_least_one_value() {
        assert_eq!(depth_steps(0.0, 10.0, 5.0), vec![0.0, 5.0, 10.0]);
        assert_eq!(depth_steps(-10.0, 0.0, 4.0), vec![-10.0, -6.0, -2.0]);
        // A single point (start == end) still yields exactly one depth, not zero.
        assert_eq!(depth_steps(3.0, 3.0, 5.0), vec![3.0]);
        // start > end is tolerated - normalized to (min, max) rather than an empty/negative range.
        assert_eq!(depth_steps(10.0, 0.0, 5.0), vec![0.0, 5.0, 10.0]);
    }

    #[test]
    fn resolve_depths_single_always_yields_exactly_one_depth_regardless_of_default_step() {
        let paths = write_synthetic_ct_series_for_cli(4, 1.0);
        let volume = build_volume(&paths).unwrap();
        let depths = resolve_depths(DepthSpec::Single(-7.0), &volume, [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], volume.center(), 2.0);
        assert_eq!(depths, vec![-7.0]);
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn resolve_depths_all_spans_the_volumes_own_extent_along_the_normal() {
        let paths = write_synthetic_ct_series_for_cli(6, 2.0);
        let volume = build_volume(&paths).unwrap();
        // Coronal: row_dir=[1,0,0], col_dir=[0,0,-1], normal=[0,1,0] - depth spans the volume's
        // own row-direction (AP) extent, i.e. rows * row_spacing_mm.
        let (row_dir, col_dir) = canonical_view_basis("coronal", &volume).unwrap();
        let depths = resolve_depths(DepthSpec::All { step: Some(10.0) }, &volume, row_dir, col_dir, volume.center(), 10.0);
        assert!(depths.len() >= 2, "expected multiple depths spanning the volume, got {depths:?}");
        let span = depths.last().unwrap() - depths.first().unwrap();
        let expected_span = (volume.rows.saturating_sub(1)) as f64 * volume.row_spacing_mm;
        assert!((span - expected_span).abs() < 15.0, "span {span} should approximate the volume's own extent {expected_span}");
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn resolve_mpr_output_kind_dispatches_on_extension() {
        let cli = base_cli();
        assert_eq!(resolve_mpr_output_kind(&cli, Path::new("out.nii")).unwrap(), MprOutputKind::Volume(VolumeFormat::Nifti));
        assert_eq!(resolve_mpr_output_kind(&cli, Path::new("out.nii.gz")).unwrap(), MprOutputKind::Volume(VolumeFormat::NiftiGz));
        assert_eq!(resolve_mpr_output_kind(&cli, Path::new("OUT.NRRD")).unwrap(), MprOutputKind::Volume(VolumeFormat::Nrrd));
        assert_eq!(resolve_mpr_output_kind(&cli, Path::new("out.dcm")).unwrap(), MprOutputKind::DicomSeries);
        assert_eq!(resolve_mpr_output_kind(&cli, Path::new("out.dicom")).unwrap(), MprOutputKind::DicomSeries);
        assert_eq!(resolve_mpr_output_kind(&cli, Path::new("out.png")).unwrap(), MprOutputKind::Rendered(RenderFormat::Png));
        assert_eq!(resolve_mpr_output_kind(&cli, Path::new("out.gputex")).unwrap(), MprOutputKind::Rendered(RenderFormat::Texture));
    }

    #[test]
    fn run_mpr_single_depth_output_is_unchanged_from_the_original_single_plane_behavior() {
        // The exact same invocation as run_mpr_writes_a_reformatted_image_for_a_synthetic_series
        // (no --mpr-depth at all) must still take the depths.len() == 1 path byte-for-byte.
        let paths = write_synthetic_ct_series_for_cli(4, 1.0);
        let output_path = temp_output_path("mpr-depth-default-single").with_extension("png");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        run_mpr(&cli, &paths, &output_path).unwrap();

        let metadata = fs::metadata(&output_path).unwrap();
        assert!(metadata.len() > 0);

        fs::remove_file(&output_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_depth_range_writes_multiple_numbered_png_files() {
        let paths = write_synthetic_ct_series_with_varying_intercept_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-depth-range").with_extension("png");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        cli.mpr_depth = Some("-2:2:1".to_string());
        // A fixed (not auto-normalized) window - otherwise robust per-image min-max
        // normalization can cancel out the fixture's uniform per-slice intercept shift, hiding
        // the very difference this test exists to check for.
        cli.window_center = Some(500.0);
        cli.window_width = Some(4000.0);
        run_mpr(&cli, &paths, &output_path).unwrap();

        let mut written_bytes = Vec::new();
        for index in 1..=5 {
            let frame_path = frame_output_path(&output_path, index).unwrap();
            let bytes = fs::read(&frame_path).expect("numbered frame should exist");
            assert!(!bytes.is_empty());
            written_bytes.push(bytes);
            fs::remove_file(&frame_path).ok();
        }
        // Every slice has genuinely distinct pixel content (varying RescaleIntercept fixture).
        for a in 0..written_bytes.len() {
            for b in (a + 1)..written_bytes.len() {
                assert_ne!(written_bytes[a], written_bytes[b], "frames {a} and {b} should differ");
            }
        }
        assert!(!output_path.exists(), "the base (non-numbered) path should not be written for a multi-slice stack");

        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_writes_a_single_valid_nifti_volume_for_a_depth_range() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-volume").with_extension("nii");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        cli.mpr_depth = Some("-2:2:1".to_string());
        run_mpr(&cli, &paths, &output_path).unwrap();

        let bytes = fs::read(&output_path).unwrap();
        assert_eq!(&bytes[344..348], b"n+1\0");
        let dim2 = i16::from_le_bytes(bytes[46..48].try_into().unwrap());
        assert_eq!(dim2, 5, "5 depths (-2,-1,0,1,2 step 1) should become dim[3] = 5");

        fs::remove_file(&output_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_writes_a_valid_nrrd_volume() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-volume").with_extension("nrrd");

        let mut cli = base_cli();
        cli.mpr = Some("coronal".to_string());
        cli.mpr_depth = Some("all:5".to_string());
        run_mpr(&cli, &paths, &output_path).unwrap();

        let bytes = fs::read(&output_path).unwrap();
        let header_end = bytes.windows(2).position(|w| w == b"\n\n").unwrap() + 2;
        let header_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
        assert!(header_text.starts_with("NRRD0004\n"));
        assert!(header_text.contains("space: left-posterior-superior\n"));

        fs::remove_file(&output_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_writes_a_valid_texture_export_with_a_metadata_sidecar() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-texture").with_extension("gputex");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        cli.window_center = Some(40.0);
        cli.window_width = Some(400.0);
        run_mpr(&cli, &paths, &output_path).unwrap();

        let sidecar_path = {
            let mut name = output_path.as_os_str().to_owned();
            name.push(".json");
            PathBuf::from(name)
        };
        let meta: serde_json::Value = serde_json::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(meta["contentKind"], "volume");
        assert_eq!(meta["compression"], "gzip");
        assert_eq!(meta["width"], 512);
        assert_eq!(meta["height"], 512);
        assert_eq!(meta["depth"], 6);
        assert_eq!(meta["defaultWindowCenter"], 40.0);
        assert_eq!(meta["defaultWindowWidth"], 400.0);
        assert_eq!(meta["downsampled"], false);
        let payload_bytes_raw = meta["payloadBytesRaw"].as_u64().unwrap();
        assert_eq!(payload_bytes_raw, 512 * 512 * 6 * 2);

        let compressed = fs::read(&output_path).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decompressed).unwrap();
        assert_eq!(decompressed.len() as u64, payload_bytes_raw);

        fs::remove_file(&output_path).ok();
        fs::remove_file(&sidecar_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_texture_export_downsamples_when_requested() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-texture-downsampled").with_extension("gputex");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        cli.texture_max_dim = Some(64);
        cli.texture_compression = Some(TextureCompressionArg::None);
        run_mpr(&cli, &paths, &output_path).unwrap();

        let sidecar_path = {
            let mut name = output_path.as_os_str().to_owned();
            name.push(".json");
            PathBuf::from(name)
        };
        let meta: serde_json::Value = serde_json::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(meta["downsampled"], true);
        assert_eq!(meta["compression"], "none");
        assert!(meta["width"].as_u64().unwrap() <= 64);
        assert!(meta["height"].as_u64().unwrap() <= 64);
        assert!(meta["depth"].as_u64().unwrap() <= 64);
        assert_eq!(meta["nativeDims"], serde_json::json!([512, 512, 6]));

        fs::remove_file(&output_path).ok();
        fs::remove_file(&sidecar_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_texture_export_rejects_reformat_only_flags() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-texture-conflict").with_extension("gputex");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        cli.mpr_depth = Some("-2:2:1".to_string());
        assert!(run_mpr(&cli, &paths, &output_path).is_err());
        assert!(!output_path.exists());

        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_writes_a_single_spatially_valid_dicom_file_for_a_single_depth() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-single-dicom").with_extension("dcm");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        run_mpr(&cli, &paths, &output_path).unwrap();

        let object = read_dicom_file(&output_path).unwrap();
        // Multi-frame Grayscale Word Secondary Capture Image Storage - see secondary_capture.rs.
        assert_eq!(object.meta().media_storage_sop_class_uid(), "1.2.840.10008.5.1.4.1.1.7.3");
        assert_eq!(object.element(tags::ROWS).unwrap().uint16().unwrap(), 512);

        fs::remove_file(&output_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_dicom_series_output_shares_one_series_instance_uid_across_numbered_files() {
        let paths = write_synthetic_ct_series_for_cli(6, 1.0);
        let output_path = temp_output_path("mpr-dicom-series").with_extension("dcm");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        cli.mpr_depth = Some("-1:1:1".to_string());
        run_mpr(&cli, &paths, &output_path).unwrap();

        let mut series_uids = Vec::new();
        let mut instance_numbers = Vec::new();
        for index in 1..=3 {
            let frame_path = frame_output_path(&output_path, index).unwrap();
            let object = read_dicom_file(&frame_path).unwrap();
            series_uids.push(object.element(tags::SERIES_INSTANCE_UID).unwrap().to_str().unwrap().into_owned());
            instance_numbers.push(object.element(tags::INSTANCE_NUMBER).unwrap().to_str().unwrap().into_owned());
            fs::remove_file(&frame_path).ok();
        }
        assert_eq!(series_uids[0], series_uids[1]);
        assert_eq!(series_uids[1], series_uids[2]);
        assert_eq!(instance_numbers, vec!["1", "2", "3"]);

        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_rejects_output_type_combined_with_a_volume_or_dicom_series_extension() {
        let paths = write_synthetic_ct_series_for_cli(4, 1.0);

        let mut nifti_cli = base_cli();
        nifti_cli.mpr = Some("axial".to_string());
        nifti_cli.output_type = Some(OutputType::Png);
        let nifti_output = temp_output_path("mpr-conflict").with_extension("nii");
        assert!(run_mpr(&nifti_cli, &paths, &nifti_output).is_err());

        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn parses_multiple_positional_paths_directly_as_a_shell_glob_would_expand_them() {
        // No --files/-I needed - a shell glob (series_dir/*.dcm) just expands into extra bare
        // positional args, exactly like INPUT/OUTPUT already do for a single file.
        let matches = Cli::command()
            .try_get_matches_from([
                "dcmnorm", "--mpr", "axial", "a.dcm", "b.dcm", "c.dcm", "out.png",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert_eq!(cli.mpr.as_deref(), Some("axial"));
        assert!(!cli.stdin_paths);
        assert_eq!(
            cli.paths,
            vec![
                PathBuf::from("a.dcm"),
                PathBuf::from("b.dcm"),
                PathBuf::from("c.dcm"),
                PathBuf::from("out.png"),
            ]
        );
    }

    #[test]
    fn finalize_derives_legacy_input_output_only_for_zero_one_or_two_paths() {
        let zero = Cli { paths: vec![], ..base_cli() }.finalize();
        assert_eq!(zero.input, None);
        assert_eq!(zero.output, None);

        let one = Cli { paths: vec![PathBuf::from("a.dcm")], ..base_cli() }.finalize();
        assert_eq!(one.input, Some(PathBuf::from("a.dcm")));
        assert_eq!(one.output, None);

        let two = Cli { paths: vec![PathBuf::from("a.dcm"), PathBuf::from("b.json")], ..base_cli() }.finalize();
        assert_eq!(two.input, Some(PathBuf::from("a.dcm")));
        assert_eq!(two.output, Some(PathBuf::from("b.json")));

        // 3+ paths is multi-input territory (batch or --mpr) - run() reads `paths` directly for
        // that, not these derived fields, so finalize() deliberately leaves output unset here.
        let three = Cli {
            paths: vec![PathBuf::from("a.dcm"), PathBuf::from("b.dcm"), PathBuf::from("c.dcm")],
            ..base_cli()
        }
        .finalize();
        assert_eq!(three.input, Some(PathBuf::from("a.dcm")));
        assert_eq!(three.output, None);
    }

    #[test]
    fn stdin_paths_allows_at_most_one_extra_positional_for_mpr_output() {
        // Parses fine with any number of positionals - run() is what actually rejects 2+ extra
        // positionals alongside -I (covered by manual verification, since run() itself reads
        // real stdin/argv and isn't unit-tested independently of the functions it dispatches to,
        // matching this file's existing convention for run()).
        let matches = Cli::command()
            .try_get_matches_from(["dcmnorm", "-I", "--mpr", "axial", "out.png"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();
        assert!(cli.stdin_paths);
        assert_eq!(cli.mpr.as_deref(), Some("axial"));
        assert_eq!(cli.paths, vec![PathBuf::from("out.png")]);
    }

    fn write_synthetic_ct_series_for_cli(count: usize, spacing_mm: f64) -> Vec<PathBuf> {
        let base = read_dicom_file(fixture_path("ct.dcm")).unwrap();
        (0..count)
            .map(|index| {
                let mut object = base.clone();
                let z = 1115.0 + (index as f64) * spacing_mm;
                object.put(DataElement::new(
                    tags::IMAGE_POSITION_PATIENT,
                    VR::DS,
                    PrimitiveValue::from(format!("-151.493508\\-36.6564417\\{z}")),
                ));
                let path = temp_output_path(&format!("mpr-cli-slice-{index}")).with_extension("dcm");
                write_dicom_file(&mut object, &path).unwrap();
                path
            })
            .collect()
    }

    /// Like write_synthetic_ct_series_for_cli, but each slice also gets a distinct
    /// RescaleIntercept (baked into that slice's own values during volume building - see
    /// dicom_io::volume's own "bake modality LUT per-slice before storage" design) - unlike the
    /// plain helper (which reuses the SAME pixel content for every "slice", differing only in
    /// ImagePositionPatient), this makes different depths/slabs through the resulting volume
    /// genuinely distinguishable in absolute value, not just position.
    fn write_synthetic_ct_series_with_varying_intercept_for_cli(count: usize, spacing_mm: f64) -> Vec<PathBuf> {
        let base = read_dicom_file(fixture_path("ct.dcm")).unwrap();
        (0..count)
            .map(|index| {
                let mut object = base.clone();
                let z = 1115.0 + (index as f64) * spacing_mm;
                object.put(DataElement::new(
                    tags::IMAGE_POSITION_PATIENT,
                    VR::DS,
                    PrimitiveValue::from(format!("-151.493508\\-36.6564417\\{z}")),
                ));
                let intercept = -1000.0 + (index as f64) * 500.0;
                object.put(DataElement::new(
                    tags::RESCALE_INTERCEPT,
                    VR::DS,
                    PrimitiveValue::from(format!("{intercept}")),
                ));
                let path = temp_output_path(&format!("mpr-cli-varying-slice-{index}")).with_extension("dcm");
                write_dicom_file(&mut object, &path).unwrap();
                path
            })
            .collect()
    }

    #[test]
    fn run_mpr_rejects_an_empty_path_list() {
        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());
        let output_path = temp_output_path("mpr-empty").with_extension("png");

        let result = run_mpr(&cli, &[], &output_path);
        assert!(result.is_err());
    }

    #[test]
    fn run_mpr_writes_a_reformatted_image_for_a_synthetic_series() {
        let paths = write_synthetic_ct_series_for_cli(4, 1.0);
        let output_path = temp_output_path("mpr-axial").with_extension("png");

        let mut cli = base_cli();
        cli.mpr = Some("axial".to_string());

        run_mpr(&cli, &paths, &output_path).unwrap();

        let metadata = fs::metadata(&output_path).unwrap();
        assert!(metadata.len() > 0);

        fs::remove_file(&output_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_writes_a_reformatted_image_for_a_rotation_triplet() {
        let paths = write_synthetic_ct_series_for_cli(4, 1.0);
        let output_path = temp_output_path("mpr-oblique").with_extension("png");

        let mut cli = base_cli();
        cli.mpr = Some("15,30,0".to_string());

        run_mpr(&cli, &paths, &output_path).unwrap();

        let metadata = fs::metadata(&output_path).unwrap();
        assert!(metadata.len() > 0);

        fs::remove_file(&output_path).ok();
        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn run_mpr_rejects_unknown_mpr_view() {
        let paths = write_synthetic_ct_series_for_cli(3, 1.0);
        let mut cli = base_cli();
        let output_path = temp_output_path("mpr-bad-view").with_extension("png");
        cli.mpr = Some("frontal".to_string());

        let result = run_mpr(&cli, &paths, &output_path);
        assert!(result.is_err());

        for path in &paths {
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn filtered_dicom_to_json_can_emit_requested_file_meta_attribute() {
        let requests = parse_filter_requests(&["MediaStorageSOPClassUID".to_string()]).unwrap();
        let input_path = fixture_path("dx.dcm");
        let mut object = read_dicom_file(&input_path).unwrap();
        apply_filter_to_object(&mut object, &requests);

        let mut cli = base_cli();
        let output_path = temp_output_path("filtered-file-meta-to-json");
        cli.output = Some(output_path.clone());
        cli.filter = vec!["MediaStorageSOPClassUID".to_string()];

        run_dicom_to_json_with_object(&cli, Path::new(&input_path), None, object).unwrap();

        let json = fs::read_to_string(&output_path).unwrap();
        let _ = fs::remove_file(&output_path);
        assert!(json.contains("\"MediaStorageSOPClassUID\""));
        assert!(!json.contains("\"StudyInstanceUID\""));
    }

    #[test]
    fn filtered_process_one_uses_bulk_data_uri_for_pixel_data() {
        let input_path = fixture_path("dx.dcm");
        let output_path = temp_output_path("filtered-pixeldata-uri");

        let mut cli = base_cli();
        cli.output = Some(output_path.clone());
        cli.filter = vec!["PixelData".to_string()];

        super::process_one(&cli, &input_path).unwrap();

        let json = fs::read_to_string(&output_path).unwrap();
        let _ = fs::remove_file(&output_path);

        assert!(json.contains("\"PixelData\""));
        assert!(json.contains("\"BulkDataURI\""));
        assert!(!json.contains("\"InlineBinary\""));
    }

    #[test]
    fn detects_mp4_output_as_render() {
        assert_eq!(
            detect_output_kind(&PathBuf::from("out.mp4"), None),
            Some(FileKind::Render)
        );
    }

    #[test]
    fn detects_mpeg4_output_as_render() {
        assert_eq!(
            detect_output_kind(&PathBuf::from("out.mpeg4"), None),
            Some(FileKind::Render)
        );
    }

    #[test]
    fn detects_mov_output_as_render() {
        assert_eq!(
            detect_output_kind(&PathBuf::from("out.mov"), None),
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
    fn infers_mpeg4_from_mpeg4_extension() {
        let cli = base_cli();
        let format = resolve_render_format(&cli, &PathBuf::from("out.mpeg4")).unwrap();
        assert_eq!(format, RenderFormat::Mpeg4);
    }

    #[test]
    fn infers_mpeg4_from_mov_extension() {
        let cli = base_cli();
        let format = resolve_render_format(&cli, &PathBuf::from("out.mov")).unwrap();
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
        let error = parse_attribute_override("SOPClassUID")
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected KEY=VALUE"));
    }

    #[test]
    fn parses_redact_box_with_negative_offsets() {
        let bbox = parse_redact_box("-20,-10,15,8").unwrap();
        assert_eq!(bbox.x, -20);
        assert_eq!(bbox.y, -10);
        match bbox.width {
            super::BoxLength::Pixels(value) => assert_eq!(value, 15),
            _ => panic!("expected pixel width"),
        }
        match bbox.height {
            super::BoxLength::Pixels(value) => assert_eq!(value, 8),
            _ => panic!("expected pixel height"),
        }
    }

    #[test]
    fn rejects_negative_redact_box_width_or_height() {
        let error = parse_redact_box("0,0,-5,10").unwrap_err().to_string();
        assert!(error.contains("W must be a non-negative integer or percentage like 25%"));
    }

    #[test]
    fn parses_redact_box_with_percentage_extents() {
        let bbox = parse_redact_box("-20,-10,25%,50%").unwrap();
        match bbox.width {
            super::BoxLength::Percent(value) => assert!((value - 25.0).abs() < f64::EPSILON),
            _ => panic!("expected percent width"),
        }
        match bbox.height {
            super::BoxLength::Percent(value) => assert!((value - 50.0).abs() < f64::EPSILON),
            _ => panic!("expected percent height"),
        }
    }

    #[test]
    fn cli_accepts_hyphen_prefixed_redact_box_value() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dcmnorm",
                "--redact-box",
                "-0,-0,20%,20%",
                "in.dcm",
                "out.jpg",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

        assert_eq!(cli.redact_box, vec!["-0,-0,20%,20%".to_string()]);
    }

    #[test]
    fn preserves_negative_zero_anchor_coordinates() {
        let bbox = parse_redact_box("-0,0,10,10").unwrap();
        assert_eq!(bbox.x, super::NEGATIVE_ZERO_ANCHOR);
        assert_eq!(bbox.y, 0);
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
        let cli = Cli::from_arg_matches(&matches).unwrap().finalize();

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
        cli.set
            .push("SOPClassUID=1.2.840.10008.5.1.4.1.1.2".to_string());

        let mut input_bytes = vec![0u8; 132];
        input_bytes[128..132].copy_from_slice(b"DICM");

        let direction = infer_direction(&cli, &PathBuf::from("in.dcm"), &input_bytes).unwrap();
        assert_eq!(direction, Direction::DicomToDicom);
    }

    #[test]
    fn infers_dicom_to_dicom_for_redaction_without_transfer_syntax() {
        let mut cli = base_cli();
        cli.output = Some(PathBuf::from("out.dcm"));
        cli.redact_box.push("0,0,10,10".to_string());

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
        cli.set
            .push("SOPClassUID=1.2.840.10008.5.1.4.1.1.2".to_string());

        let mut input_bytes = vec![0u8; 132];
        input_bytes[128..132].copy_from_slice(b"DICM");

        let error = infer_direction(&cli, &PathBuf::from("in.dcm"), &input_bytes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be combined with an explicit output path"));
    }
}
