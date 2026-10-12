//! Command-line tools for inspecting scanners and testing scans without the GUI.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use scanclog::escl::{
    self, Client, ColorMode, Endpoint, InputSource, Intent, ScanRegion, ScanSettings, ScannerInfo,
    UNITS_PER_INCH,
};
use scanclog::export::{self, FileFormat, PdfImage, PdfOptions};
use scanclog::imaging::{self, Adjustments, Crop, OutputMode, Raster, Rotation, Target, Threshold};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Discover scanners and print their capabilities and status.
    Probe(ProbeArgs),
    /// Run a scan job and save the pages exactly as the scanner returns them.
    Scan(ScanArgs),
    /// Apply ScanClog's processing to an existing image file.
    Process(ProcessArgs),
}

#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// Probe this address instead of discovering scanners on the network.
    #[arg(long)]
    host: Option<String>,
    /// How long to wait for mDNS answers, in seconds.
    #[arg(long, default_value_t = 3)]
    timeout: u64,
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Scanner address (IP, host[:port] or URL). Defaults to the only scanner discovered.
    #[arg(long)]
    host: Option<String>,
    #[arg(long, value_enum, default_value_t = SourceArg::Platen)]
    source: SourceArg,
    #[arg(long, value_enum, default_value_t = ModeArg::Color)]
    mode: ModeArg,
    /// Resolution in dpi.
    #[arg(long, default_value_t = 300)]
    dpi: u32,
    /// Scan area as X,Y,WIDTH,HEIGHT in millimetres. Defaults to the full bed.
    #[arg(long, value_parser = parse_region)]
    region: Option<RegionMm>,
    #[arg(long, value_enum)]
    intent: Option<IntentArg>,
    /// eSCL CompressionFactor, if the scanner supports it.
    #[arg(long)]
    compression: Option<i32>,
    /// Output file. With several pages, a page number is appended to the name.
    #[arg(short, long, default_value = "scan.jpg")]
    output: PathBuf,
}

#[derive(Debug, Args)]
pub struct ProcessArgs {
    /// Input images, typically JPEGs saved by `scanclog scan`. Several
    /// inputs become the pages of one PDF.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
    /// Resolution the input was scanned at.
    #[arg(long, default_value_t = 300)]
    dpi: u32,
    #[arg(long, value_enum, default_value_t = OutputArg::Color)]
    mode: OutputArg,
    /// Black-and-white threshold, 0-255, or "auto".
    #[arg(long, default_value = "auto", value_parser = parse_threshold)]
    threshold: Threshold,
    /// -1.0 ..= 1.0
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    brightness: f32,
    /// -1.0 ..= 1.0
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    contrast: f32,
    /// 0.0 ..= 2.0
    #[arg(long, default_value_t = 0.0)]
    sharpen: f32,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    #[arg(long, default_value_t = 0, value_parser = parse_rotation)]
    rotate: u16,
    /// Fine clockwise rotation in degrees to straighten a tilted page, or
    /// "auto" to detect it.
    #[arg(long, value_parser = parse_straighten, allow_negative_numbers = true)]
    straighten: Option<Straighten>,
    /// Crop as LEFT,TOP,RIGHT,BOTTOM fractions (0-1) of the rotated image,
    /// or "auto" to detect the document on the scanner glass.
    #[arg(long, value_parser = parse_crop)]
    crop: Option<CropArg>,
    /// Output file; the format follows the extension (.pdf, .png, .jpg).
    #[arg(short, long, default_value = "processed.pdf")]
    output: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputArg {
    Color,
    Gray,
    Bw,
}

fn parse_threshold(value: &str) -> Result<Threshold, String> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(Threshold::Auto);
    }
    value
        .parse()
        .map(Threshold::Manual)
        .map_err(|_| "expected 0-255 or 'auto'".to_owned())
}

#[derive(Debug, Clone, Copy)]
enum Straighten {
    Degrees(f32),
    Auto,
}

fn parse_straighten(value: &str) -> Result<Straighten, String> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(Straighten::Auto);
    }
    match value.parse::<f32>() {
        Ok(degrees) if degrees.abs() <= 45.0 => Ok(Straighten::Degrees(degrees)),
        _ => Err("expected an angle between -45 and 45 degrees, or 'auto'".to_owned()),
    }
}

fn parse_rotation(value: &str) -> Result<u16, String> {
    match value.parse() {
        Ok(degrees @ (0 | 90 | 180 | 270)) => Ok(degrees),
        _ => Err("expected 0, 90, 180 or 270".to_owned()),
    }
}

#[derive(Debug, Clone, Copy)]
enum CropArg {
    Area(Crop),
    Auto,
}

fn parse_crop(value: &str) -> Result<CropArg, String> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(CropArg::Auto);
    }
    let parts: Vec<f32> = value
        .split(',')
        .map(|p| p.trim().parse::<f32>().map_err(|e| format!("'{p}': {e}")))
        .collect::<Result<_, _>>()?;
    match parts[..] {
        [left, top, right, bottom] if left < right && top < bottom => Ok(CropArg::Area(
            Crop::from_corners((left, top), (right, bottom)),
        )),
        _ => Err("expected LEFT,TOP,RIGHT,BOTTOM with left < right and top < bottom".into()),
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    Platen,
    Feeder,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    Color,
    Gray,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum IntentArg {
    Document,
    Text,
    Photo,
    Preview,
}

#[derive(Debug, Clone, Copy)]
struct RegionMm {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn parse_region(value: &str) -> Result<RegionMm, String> {
    let parts: Vec<f64> = value
        .split(',')
        .map(|p| p.trim().parse::<f64>().map_err(|e| format!("'{p}': {e}")))
        .collect::<Result<_, _>>()?;
    match parts[..] {
        [x, y, width, height] if parts.iter().all(|v| *v >= 0.0) && width > 0.0 && height > 0.0 => {
            Ok(RegionMm {
                x,
                y,
                width,
                height,
            })
        }
        _ => Err("expected four non-negative numbers: X,Y,WIDTH,HEIGHT".into()),
    }
}

fn mm_to_units(mm: f64) -> u32 {
    (mm / 25.4 * f64::from(UNITS_PER_INCH)).round() as u32
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Probe(args) => probe(args),
        Command::Scan(args) => scan(args),
        Command::Process(args) => process(args),
    }
}

fn probe(args: ProbeArgs) -> Result<()> {
    let scanners = match args.host {
        Some(host) => vec![ScannerInfo::manual(Endpoint::parse(&host)?)],
        None => {
            eprintln!("Searching for scanners for {} s...", args.timeout);
            escl::discover(Duration::from_secs(args.timeout), |_| {})?
        }
    };
    if scanners.is_empty() {
        bail!("no eSCL scanner found; try --host ADDRESS");
    }
    for scanner in scanners {
        println!("{}", scanner.name);
        if let Some(uuid) = &scanner.uuid {
            println!("  uuid: {uuid}");
        }
        for endpoint in &scanner.endpoints {
            println!("  endpoint: {endpoint}");
        }
        let client = Client::new(scanner.endpoints[0].clone());
        match client.capabilities() {
            Ok(caps) => print_capabilities(&caps),
            Err(err) => println!("  capabilities: error: {err}"),
        }
        match client.status() {
            Ok(status) => println!(
                "  status: {:?}, feeder: {:?}",
                status.state, status.adf_state
            ),
            Err(err) => println!("  status: error: {err}"),
        }
    }
    Ok(())
}

fn print_capabilities(caps: &escl::Capabilities) {
    println!(
        "  model: {} (eSCL {})",
        caps.make_and_model.as_deref().unwrap_or("?"),
        caps.version.as_deref().unwrap_or("?")
    );
    for source in caps.sources() {
        let Some(s) = caps.source(source) else {
            continue;
        };
        let to_mm = |units: u32| f64::from(units) * 25.4 / f64::from(UNITS_PER_INCH);
        println!("  {}:", source.label());
        println!(
            "    area: {:.0} x {:.0} mm, dpi: {:?}",
            to_mm(s.max_width),
            to_mm(s.max_height),
            s.resolutions
        );
        println!(
            "    modes: {:?}, formats: {:?}",
            s.color_modes, s.document_formats
        );
        println!("    intents: {:?}", s.intents);
    }
    if caps.feeder.is_some() {
        println!(
            "  feeder: duplex {}, capacity {:?}",
            caps.duplex, caps.feeder_capacity
        );
    }
}

fn scan(args: ScanArgs) -> Result<()> {
    let endpoint = match &args.host {
        Some(host) => Endpoint::parse(host)?,
        None => {
            let mut scanners = escl::discover(Duration::from_secs(3), |_| {})?;
            match scanners.len() {
                0 => bail!("no eSCL scanner found; use --host ADDRESS"),
                1 => scanners.remove(0).endpoints.remove(0),
                _ => bail!("several scanners found; choose one with --host (see `scanclog probe`)"),
            }
        }
    };
    let client = Client::new(endpoint);
    let caps = client
        .capabilities()
        .context("reading scanner capabilities")?;

    let source = match args.source {
        SourceArg::Platen => InputSource::Platen,
        SourceArg::Feeder => InputSource::Feeder,
    };
    let source_caps = caps
        .source(source)
        .with_context(|| format!("the scanner has no {}", source.label().to_lowercase()))?;
    let color_mode = match args.mode {
        ModeArg::Color => ColorMode::Rgb24,
        ModeArg::Gray => ColorMode::Grayscale8,
    };
    if !source_caps.supports_color_mode(color_mode) {
        bail!("the scanner does not support {color_mode}");
    }
    if !source_caps.resolutions.contains(&args.dpi) {
        bail!(
            "unsupported resolution {} dpi; available: {:?}",
            args.dpi,
            source_caps.resolutions
        );
    }

    let mut settings = ScanSettings::new(source, color_mode, args.dpi);
    if let Some(version) = &caps.version {
        settings.version.clone_from(version);
    }
    settings.region = Some(match args.region {
        Some(r) => ScanRegion {
            x: mm_to_units(r.x),
            y: mm_to_units(r.y),
            width: mm_to_units(r.width),
            height: mm_to_units(r.height),
        },
        None => ScanRegion::full(source_caps.max_width, source_caps.max_height),
    });
    settings.intent = args.intent.map(|i| match i {
        IntentArg::Document => Intent::Document,
        IntentArg::Text => Intent::TextAndGraphic,
        IntentArg::Photo => Intent::Photo,
        IntentArg::Preview => Intent::Preview,
    });
    settings.compression_factor = args.compression;

    let started = Instant::now();
    let job = client
        .create_job(&settings)
        .context("creating the scan job")?;
    log::info!("job created at {}", job.url());

    let mut pages = 0;
    while let Some(document) = job.next_document()? {
        pages += 1;
        let path = numbered_path(&args.output, pages);
        std::fs::write(&path, &document.data)
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "page {pages}: {} ({} KiB, {}) after {:.1} s",
            path.display(),
            document.data.len() / 1024,
            document.content_type.as_deref().unwrap_or("unknown type"),
            started.elapsed().as_secs_f64()
        );
    }
    if pages == 0 {
        bail!("the scanner returned no pages");
    }
    Ok(())
}

/// `scan.jpg` stays as is for page 1 and becomes `scan-2.jpg` and so on afterwards.
fn numbered_path(path: &Path, page: usize) -> PathBuf {
    if page == 1 {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!("{stem}-{page}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{page}"),
    };
    path.with_file_name(name)
}

fn process(args: ProcessArgs) -> Result<()> {
    let format = FileFormat::from_path(&args.output)
        .context("unsupported output format; use a .pdf, .png or .jpg file name")?;
    if args.inputs.len() > 1 && format != FileFormat::Pdf {
        bail!("several inputs can only be combined into a PDF");
    }
    let mut adjustments = Adjustments {
        mode: match args.mode {
            OutputArg::Color => OutputMode::Color,
            OutputArg::Gray => OutputMode::Gray,
            OutputArg::Bw => OutputMode::BlackAndWhite,
        },
        rotation: match args.rotate {
            90 => Rotation::Cw90,
            180 => Rotation::Cw180,
            270 => Rotation::Cw270,
            _ => Rotation::None,
        },
        straighten: match args.straighten {
            Some(Straighten::Degrees(degrees)) => degrees,
            _ => 0.0,
        },
        crop: match args.crop {
            Some(CropArg::Area(crop)) => crop,
            _ => Crop::FULL,
        },
        brightness: args.brightness,
        contrast: args.contrast,
        sharpen: args.sharpen,
        threshold: args.threshold,
    };

    let mut pages = Vec::new();
    for input in &args.inputs {
        let data = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
        let started = Instant::now();
        let source = imaging::decode(&data, args.dpi as f32)?;
        let decoded = started.elapsed();
        if let Some(Straighten::Auto) = args.straighten {
            let gray = imaging::render(
                &source,
                &Adjustments {
                    mode: OutputMode::Gray,
                    ..Adjustments::default()
                },
                Target::Full,
            );
            let Raster::Gray(gray) = gray.raster else {
                unreachable!("gray mode renders gray")
            };
            adjustments.straighten = imaging::detect_skew(&gray).unwrap_or(0.0);
            eprintln!(
                "{}: straightening by {:.2}°",
                input.display(),
                adjustments.straighten
            );
        }
        if let Some(CropArg::Auto) = args.crop {
            // Detect on the image as it will be shown: rotated and straightened.
            let view = Adjustments {
                mode: OutputMode::Color,
                crop: Crop::FULL,
                ..adjustments
            };
            let view = imaging::render(&source, &view, Target::Full);
            adjustments.crop = imaging::detect_document(&view).unwrap_or(Crop::FULL);
            let c = adjustments.crop;
            eprintln!(
                "{}: document at {:.3},{:.3},{:.3},{:.3}",
                input.display(),
                c.left,
                c.top,
                c.right,
                c.bottom
            );
        }
        pages.push(imaging::render(&source, &adjustments, Target::Full));
        eprintln!(
            "{}: decoded in {:.0} ms, rendered in {:.0} ms",
            input.display(),
            decoded.as_secs_f64() * 1000.0,
            (started.elapsed() - decoded).as_secs_f64() * 1000.0
        );
    }

    let started = Instant::now();
    let data = match format {
        FileFormat::Pdf => {
            export::write_pdf(pages.iter().map(PdfImage::Rendered), &PdfOptions::default())?
        }
        FileFormat::Png => export::encode_png(&pages[0])?,
        FileFormat::Jpeg => export::encode_jpeg(&pages[0], PdfOptions::default().jpeg_quality)?,
    };
    export::write_atomically(&args.output, &data)
        .with_context(|| format!("writing {}", args.output.display()))?;
    eprintln!(
        "{}: {} KiB, encoded in {:.0} ms",
        args.output.display(),
        data.len() / 1024,
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}
