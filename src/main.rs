use anyhow::{Context, Result};
use clap::Parser;
use log::{debug, error, info};
use log4rs;
use mkv_remuxer::sink::{FileSink, OutputSink};
use mkv_remuxer::source::{FileSource, InputSource, SeekType, WebVttSource};
use mkv_remuxer::*;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(name = env!("CARGO_PKG_NAME"))]
#[command(author, version, about = "MKV remuxer with cutting and track mapping support", long_about = None)]
struct Args {
    /// Input file(s). Can be specified multiple times: -i input1.mkv -i input2.mkv
    #[arg(short = 'i', long = "input", required = true)]
    inputs: Vec<String>,

    /// Start position (e.g., "5s", "1m30s", "90", "1:30")
    /// Supports: seconds (s), minutes (m), or MM:SS format
    #[arg(short = 's', long = "ss")]
    start: Option<String>,

    /// Duration from start (e.g., "10s", "2m", "120")
    /// Cannot be used with --to
    #[arg(short = 't', long)]
    duration: Option<String>,

    /// End position (e.g., "15s", "2m30s", "150")
    /// Cannot be used with -t/--duration
    #[arg(long = "to")]
    end: Option<String>,

    /// Seek/cut mode: freeze, squeeze, snap, dirty
    /// - freeze: Freeze frame before cut point (fast, visible artifact)
    /// - squeeze: Compress pre-roll frames (slow decode, seamless)
    /// - snap: Snap to nearest keyframe (fast, inexact timing)
    /// - dirty: Cut at exact position (may cause decoding issues)
    #[arg(long = "seek-mode", default_value = "freeze")]
    seek_mode: String,

    /// Track mappings in format "source:track" (e.g., "0:1" for track 1 from first input)
    /// Can be specified multiple times: -map 0:1 -map 0:2 -map 1:1
    /// If not specified, defaults to first video + all audio/subtitle tracks
    #[arg(short = 'm', long = "map")]
    mappings: Vec<String>,

    /// Output file path
    #[arg(required = true)]
    output: String,

    /// Verbose output
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
}

/// Initialize log4rs logging with appropriate log level
fn init_logging(verbose: bool) -> Result<()> {
    use log::LevelFilter;
    use log4rs::append::console::{ConsoleAppender, Target};
    use log4rs::config::{Appender, Config, Root};
    use log4rs::encode::pattern::PatternEncoder;

    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let console = ConsoleAppender::builder()
        .target(Target::Stderr)
        .encoder(Box::new(PatternEncoder::new(
            "{d(%Y-%m-%d %H:%M:%S)} [{l}] {m}{n}",
        )))
        .build();

    let config = Config::builder()
        .appender(Appender::builder().build("console", Box::new(console)))
        .build(Root::builder().appender("console").build(level))?;

    log4rs::init_config(config)?;

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    init_logging(args.verbose)?;

    info!("Input files: {:?}", args.inputs);
    info!("Output file: {}", args.output);
    info!("Seek mode: {}", args.seek_mode);

    // Validate mutually exclusive options
    if args.duration.is_some() && args.end.is_some() {
        anyhow::bail!("Cannot specify both --duration (-t) and --to at the same time");
    }

    // Parse seek mode
    let seek_type = match args.seek_mode.as_str() {
        "squeeze" => SeekType::Squeeze,
        "snap" => SeekType::SnapNearestKeyframe,
        "dirty" => SeekType::DirtyCut,
        _ => anyhow::bail!(
            "Invalid seek mode: {}. Valid options: squeeze, snap, dirty",
            args.seek_mode
        ),
    };

    // Parse start time
    let start_ns = args
        .start
        .as_ref()
        .map(|s| parse_time(s))
        .transpose()
        .context("Failed to parse start time")?;

    // Parse end time or calculate from duration
    let end_ns = if let Some(end_str) = &args.end {
        Some(parse_time(end_str).context("Failed to parse end time")?)
    } else if let (Some(start), Some(dur_str)) = (start_ns, &args.duration) {
        let duration = parse_time(dur_str).context("Failed to parse duration")?;
        Some(start + duration)
    } else {
        None
    };

    if let Some(start) = start_ns {
        info!("Start time: {:.3}s", start as f64 / 1_000_000_000.0);
    }
    if let Some(end) = end_ns {
        info!("End time: {:.3}s", end as f64 / 1_000_000_000.0);
    }

    // Create cut config if needed
    let cut_interval = if start_ns.is_some() || end_ns.is_some() {
        let mut config = CutInterval::new();
        if let Some(start) = start_ns {
            config = config.with_start(start);
        }
        if let Some(end) = end_ns {
            config = config.with_end(end);
        }
        Some(config)
    } else {
        None
    };

    // Parse track mappings
    let track_mappings = if !args.mappings.is_empty() {
        let mut mappings = Vec::new();
        for mapping_str in &args.mappings {
            mappings.push(
                parse_mapping(mapping_str)
                    .with_context(|| format!("Failed to parse mapping: {}", mapping_str))?,
            );
        }
        Some(mappings)
    } else {
        None
    };

    if let Some(ref mappings) = track_mappings {
        debug!("Track mappings: {:?}", mappings);
    }

    // Create input sources
    let mut sources = Vec::new();
    for (idx, input_path) in args.inputs.iter().enumerate() {
        info!("Loading input {}: {}", idx, input_path);
        
        // Detect file type by extension
        let path = Path::new(input_path);
        let extension = path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase());
        
        let input_source = match extension.as_deref() {
            Some("vtt") | Some("webvtt") => {
                // Create WebVTT source with default language "eng"
                // TODO: Allow language override via CLI flag
                let vtt_source = WebVttSource::new(input_path, "eng")
                    .with_context(|| format!("Failed to parse WebVTT file: {}", input_path))?;
                InputSource::from(vtt_source)
            }
            _ => {
                // Default to FileSource for .mkv, .webm, etc.
                let file_source = FileSource::new(input_path)
                    .with_context(|| format!("Failed to open input file: {}", input_path))?;
                InputSource::from(file_source)
            }
        };
        
        sources.push(input_source);
    }

    // Create output sink
    info!("Creating output: {}", args.output);
    let file_sink = FileSink::new(&args.output)
        .with_context(|| format!("Failed to create output file: {}", args.output))?;
    let output_sink = OutputSink::from(file_sink);

    // Execute remux
    info!("Starting remux...");
    let stats = remux(
        sources,
        output_sink,
        cut_interval,
        Some(seek_type.clone()),
        track_mappings
    )
    .context("Remux operation failed")?;

    // Print statistics
    info!("✓ Remux completed successfully!");
    info!("  Blocks processed: {}", stats.blocks_processed);
    info!("  Output tracks: {}", stats.track_count);
    if stats.duration_ns > 0 {
        info!(
            "  Duration: {:.3}s",
            stats.duration_ns as f64 / 1_000_000_000.0
        );
    }

    Ok(())
}

/// Parse time string to nanoseconds
/// Supports formats: "5s", "1m30s", "90", "1:30", "1:30.5"
fn parse_time(time_str: &str) -> Result<u64> {
    let time_str = time_str.trim();

    // Try MM:SS or MM:SS.mmm format
    if time_str.contains(':') {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() == 2 {
            let minutes: f64 = parts[0].parse().context("Invalid minutes in time format")?;
            let seconds: f64 = parts[1].parse().context("Invalid seconds in time format")?;
            let total_seconds = minutes * 60.0 + seconds;
            return Ok((total_seconds * 1_000_000_000.0) as u64);
        } else if parts.len() == 3 {
            // HH:MM:SS format
            let hours: f64 = parts[0].parse().context("Invalid hours in time format")?;
            let minutes: f64 = parts[1].parse().context("Invalid minutes in time format")?;
            let seconds: f64 = parts[2].parse().context("Invalid seconds in time format")?;
            let total_seconds = hours * 3600.0 + minutes * 60.0 + seconds;
            return Ok((total_seconds * 1_000_000_000.0) as u64);
        }
    }

    // Try formats with unit suffixes: "5s", "1m30s", etc.
    if time_str.contains('s') || time_str.contains('m') || time_str.contains('h') {
        let mut total_ns = 0u64;
        let mut current_num = String::new();

        for ch in time_str.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                current_num.push(ch);
            } else if ch == 'h' {
                let hours: f64 = current_num.parse().context("Invalid number before 'h'")?;
                total_ns += (hours * 3600.0 * 1_000_000_000.0) as u64;
                current_num.clear();
            } else if ch == 'm' {
                let minutes: f64 = current_num.parse().context("Invalid number before 'm'")?;
                total_ns += (minutes * 60.0 * 1_000_000_000.0) as u64;
                current_num.clear();
            } else if ch == 's' {
                let seconds: f64 = current_num.parse().context("Invalid number before 's'")?;
                total_ns += (seconds * 1_000_000_000.0) as u64;
                current_num.clear();
            }
        }

        return Ok(total_ns);
    }

    // Try plain number (assume seconds)
    let seconds: f64 = time_str
        .parse()
        .context("Invalid time format. Use formats like: 5s, 1m30s, 90, 1:30")?;
    Ok((seconds * 1_000_000_000.0) as u64)
}

/// Parse track mapping string "source:track" to (source_index, track_number)
fn parse_mapping(mapping_str: &str) -> Result<TrackMapping> {
    let parts: Vec<&str> = mapping_str.split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid mapping format. Expected 'source:track' (e.g., '0:1')");
    }

    let source_index: u64 = parts[0]
        .parse()
        .context("Invalid source index in mapping")?;
    let track_number: u64 = parts[1]
        .parse()
        .context("Invalid track number in mapping")?;

    Ok((source_index, track_number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_time_seconds() {
        assert_eq!(parse_time("5s").unwrap(), 5_000_000_000);
        assert_eq!(parse_time("5.5s").unwrap(), 5_500_000_000);
    }

    #[test]
    fn test_parse_time_minutes() {
        assert_eq!(parse_time("1m").unwrap(), 60_000_000_000);
        assert_eq!(parse_time("1m30s").unwrap(), 90_000_000_000);
    }

    #[test]
    fn test_parse_time_mmss() {
        assert_eq!(parse_time("1:30").unwrap(), 90_000_000_000);
        assert_eq!(parse_time("0:05").unwrap(), 5_000_000_000);
    }

    #[test]
    fn test_parse_time_plain_number() {
        assert_eq!(parse_time("90").unwrap(), 90_000_000_000);
        assert_eq!(parse_time("5.5").unwrap(), 5_500_000_000);
    }

    #[test]
    fn test_parse_mapping() {
        let (source, track) = parse_mapping("0:1").unwrap();
        assert_eq!(source, 0);
        assert_eq!(track, 1);
    }
}
