use crate::{Result, Error};
use crate::source::{SeekType, InputSource, Uninitialized};
use crate::sink::OutputSink;
use crate::source_mappings::SourcesMappings;
use crate::metling_pot::MeltingPot;
use log::{debug, info, warn};

/// Configuration for cutting/seeking behavior
#[derive(Debug, Clone)]
pub struct CutConfig {
    pub seek_type: SeekType,
    pub start_ns: Option<u64>,
    pub end_ns: Option<u64>,
}

impl CutConfig {
    pub fn new(seek_type: SeekType) -> Self {
        Self {
            seek_type,
            start_ns: None,
            end_ns: None,
        }
    }

    pub fn with_start(mut self, start_ns: u64) -> Self {
        self.start_ns = Some(start_ns);
        self
    }

    pub fn with_end(mut self, end_ns: u64) -> Self {
        self.end_ns = Some(end_ns);
        self
    }

    pub fn with_range(mut self, start_ns: u64, end_ns: u64) -> Self {
        self.start_ns = Some(start_ns);
        self.end_ns = Some(end_ns);
        self
    }
}

/// Track mapping specification: (source_index, track_number)
pub type TrackMapping = (u64, u64);

/// Statistics about a remuxing operation
#[derive(Debug, Default, Clone)]
pub struct RemuxStats {
    /// Total number of blocks processed
    pub blocks_processed: u64,
    /// Duration in nanoseconds
    pub duration_ns: u64,
    /// Number of tracks
    pub track_count: usize,
}

/// Remux multiple input sources into a single output sink
/// 
/// # Arguments
/// * `sources` - List of uninitialized input sources
/// * `output_sink` - Uninitialized output sink
/// * `cut_config` - Optional cut configuration (start/end times and seek type)
/// * `mappings` - Optional track mappings. If None, defaults to first video + all audio/subtitle tracks
/// * `target_timescale` - Optional target timescale for output (nanoseconds per tick)
/// 
/// # Returns
/// * `RemuxStats` - Statistics about the remuxing operation
/// 
/// # Errors
/// * Returns error if SnapNearestKeyframe is used with more than 1 output video stream
/// * Returns error if sources have incompatible timescales
/// * Returns error if track mappings are invalid
/// 
/// # Example
/// ```no_run
/// use playground_element::*;
/// use playground_element::source::{FileSource, InputSource};
/// use playground_element::sink::{FileSink, OutputSink};
/// 
/// # fn main() -> Result<()> {
/// // Create input sources
/// let source1 = InputSource::from(FileSource::new("input1.mkv")?);
/// let source2 = InputSource::from(FileSource::new("input2.mkv")?);
/// 
/// // Create output sink
/// let output = OutputSink::from(FileSink::new("output.mkv")?);
/// 
/// // Simple remux with default mappings (first video + all audio/subtitle)
/// let stats = remux(vec![source1, source2], output, None, None, None)?;
/// println!("Processed {} blocks", stats.blocks_processed);
/// 
/// // Remux with cutting
/// let source3 = InputSource::from(FileSource::new("input3.mkv")?);
/// let output2 = OutputSink::from(FileSink::new("output2.mkv")?);
/// let cut = CutConfig::new(SeekType::Freeze)
///     .with_range(5_000_000_000, 15_000_000_000); // 5s to 15s
/// let stats = remux(vec![source3], output2, Some(cut), None, None)?;
/// 
/// // Remux with custom track mappings
/// // Map track 1 from source 0 and track 2 from source 1
/// let mappings = vec![(0, 1), (1, 2)];
/// let source4 = InputSource::from(FileSource::new("input4.mkv")?);
/// let output3 = OutputSink::from(FileSink::new("output3.mkv")?);
/// let stats = remux(vec![source4], output3, None, Some(mappings), None)?;
/// # Ok(())
/// # }
/// ```
pub fn remux(
    sources: Vec<InputSource<Uninitialized>>,
    output_sink: OutputSink<crate::sink::Uninitialized>,
    cut_config: Option<CutConfig>,
    mappings: Option<Vec<TrackMapping>>,
    target_timescale: Option<u64>,
) -> Result<RemuxStats> {
    debug!("Starting remux process with {} sources", sources.len());
    
    // Step 1: Initialize sources
    let mut initialized_sources = Vec::new();
    
    for (idx, source) in sources.into_iter().enumerate() {
        debug!("Initializing source {}", idx);
        let initialized = if let Some(ref cut_config) = cut_config {
            // Initialize with cut
            let (init_source, _offsets) = source.initialize_with_cut(
                target_timescale,
                cut_config.seek_type.clone(),
                cut_config.start_ns,
                cut_config.end_ns,
            )?;
            init_source
        } else {
            // Initialize without cut
            source.initialize(target_timescale)?
        };
        initialized_sources.push(initialized);
    }

    debug!("Initialized {} sources", initialized_sources.len());

    // Step 2: Create SourcesMappings
    let mut sources_mappings = SourcesMappings::new(initialized_sources)?;

    // Step 3: Apply track mappings
    if let Some(mappings) = mappings {
        debug!("Applying {} custom track mappings", mappings.len());
        // Use custom mappings
        for (source_idx, track_num) in mappings {
            sources_mappings.add_mapping(source_idx, track_num)?;
        }
    } else {
        debug!("Using default track mappings");
        // Default mappings: first video track + all audio + all subtitle tracks
        if let Err(_) = sources_mappings.add_first_video_track() {
            debug!("No video track found");
            // No video track found, that's ok
        }
        sources_mappings.add_all_audio_tracks()?;
        sources_mappings.add_all_subtitle_tracks()?;
    }

    // Step 4: Validate keyframe snap usage
    if let Some(ref cut_config) = cut_config {
        if matches!(cut_config.seek_type, SeekType::SnapNearestKeyframe) {
            let video_count = count_video_tracks_in_mappings(&sources_mappings)?;
            if video_count > 1 {
                warn!("SnapNearestKeyframe cannot be used with {} video streams", video_count);
                return Err(Error::InvalidConfig(format!(
                    "SnapNearestKeyframe seek type cannot be used with {} video streams (>1). \
                    Use Squeeze, Freeze, or DirtyCut instead, or reduce to single video track.",
                    video_count
                )));
            }
        }
    }

    // Step 5: Get output tracks metadata
    let output_tracks = sources_mappings.get_output_tracks_metadata()?;
    debug!("Output will have {} tracks", output_tracks.track_entry.len());
    
    // Step 6: Get info and chapters from first source
    let info = sources_mappings.sources.first()
        .ok_or_else(|| Error::MissingElement("No sources available".to_string()))?
        .get_info()?;
    
    let chapters = sources_mappings.sources.first()
        .and_then(|s| s.get_chapters().ok().flatten());

    // Step 7: Initialize output sink
    debug!("Initializing output sink");
    let mut output_sink = output_sink.initialize(&output_tracks, &info, chapters.as_ref())?;

    // Step 8: Create MeltingPot and process clusters
    debug!("Starting cluster processing");
    let mut melting_pot = MeltingPot::new(sources_mappings);
    let mut blocks_processed = 0u64;
    let mut clusters_written = 0u64;

    loop {
        match melting_pot.generate_next_cluster()? {
            Some(cluster) => {
                blocks_processed += cluster.blocks.len() as u64;
                clusters_written += 1;
                if clusters_written % 100 == 0 {
                    debug!("Processed {} clusters, {} blocks", clusters_written, blocks_processed);
                }
                output_sink.write_cluster(&cluster, 0)?;
            }
            None => {
                debug!("All clusters processed");
                break;
            }
        }
    }

    // Step 9: Finalize output
    debug!("Finalizing output");
    output_sink.finalize()?;

    info!("Remux completed: {} clusters, {} blocks", clusters_written, blocks_processed);

    Ok(RemuxStats {
        blocks_processed,
        duration_ns: info.duration.map(|d| (d.0 * info.timestamp_scale.0 as f64) as u64).unwrap_or(0),
        track_count: output_tracks.track_entry.len(),
    })
}

/// Helper function to count video tracks in current mappings
fn count_video_tracks_in_mappings(sources_mappings: &SourcesMappings) -> Result<usize> {
    let output_tracks = sources_mappings.get_output_tracks_metadata()?;
    let video_count = output_tracks.track_entry.iter()
        .filter(|track| track.track_type.0 == 1) // 1 = video track type
        .count();
    Ok(video_count)
}
