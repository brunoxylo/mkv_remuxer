use crate::metling_pot::MeltingPot;
use crate::sink::{Initialized, OutputSink};
use crate::source::{CutInterval, InputSource, SeekType, Uninitialized};
use crate::source_mappings::SourcesMappings;
use crate::{Error, Result};
use log::{debug, info, warn};

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

/// A streaming remuxer that processes one cluster at a time.
///
/// Create with [`Remuxer::new`], then call [`Remuxer::process`] in a loop
/// until it returns `Err(Error::Done)`.
pub struct Remuxer {
    melting_pot: MeltingPot,
    output_sink: OutputSink<Initialized>,
    blocks_processed: u64,
    clusters_written: u64,
    track_count: usize,
    duration_ns: u64,
}

impl Remuxer {
    /// Initialize the remuxer. Returns `(Remuxer, actual_cut_interval)`.
    ///
    /// `actual_cut_interval` reflects any snapping that occurred (e.g. when
    /// `SeekType::SnapNearestKeyframe` is used); it equals the supplied
    /// `cut_interval` when no snapping is needed.
    pub fn new(
        sources: Vec<InputSource<Uninitialized>>,
        output_sink: OutputSink<crate::sink::Uninitialized>,
        cut_interval: Option<CutInterval>,
        seek_type: Option<SeekType>,
        mappings: Option<Vec<TrackMapping>>,
    ) -> Result<(Self, CutInterval)> {
        debug!("Initializing Remuxer with {} sources", sources.len());

        let seek_type = seek_type.unwrap_or(SeekType::SnapNearestKeyframe);
        let mut initialized_sources = Vec::new();
        let mut target_timescale = None;

        let mut output_interval = if let Some(ref cut_interval) = cut_interval {
            let mut new_cut_interval = cut_interval.clone();
            info!("Only remux content in interval: {}", cut_interval);
            let mut used_seek_type = seek_type.clone();
            for (idx, source) in sources.into_iter().enumerate() {
                debug!("Initializing source {} with cut", idx);
                let (init_source, offsets) = source.initialize_with_cut(
                    target_timescale,
                    used_seek_type.clone(),
                    new_cut_interval.clone(),
                )?;
                // if us efirst source as refernce for snapping and seek type is snap to nearest keyframe, update the cut interval to reflect the actual snap points (which may be different from requested cut interval)
                if matches!(seek_type, SeekType::SnapNearestKeyframe | SeekType::SnapPreviousKeyframe) && idx == 0 {
                    new_cut_interval = offsets;
                    used_seek_type = SeekType::DirtyCut;
                    info!("Actual cut interval for source {}: {:?}", idx, new_cut_interval);
                }
                if target_timescale.is_none() {
                    target_timescale = Some(init_source.get_own_timecode_scale()?);
                }
                initialized_sources.push(init_source);
            }
            new_cut_interval
        } else {
            let mut first_cut_interval = CutInterval::new().with_start(0);
            for (idx, source) in sources.into_iter().enumerate() {
                debug!("Initializing source {}", idx);
                let (src, cut_interval) = source.initialize(target_timescale)?;
                if target_timescale.is_none() {
                    target_timescale = Some(src.get_own_timecode_scale()?);
                }
                if idx == 0 {
                    first_cut_interval.start_ns = cut_interval.start_ns;
                    first_cut_interval.end_ns = cut_interval.end_ns;
                }
                initialized_sources.push(src);
            }
            first_cut_interval

        };

        let target_timescale = target_timescale
            .ok_or_else(|| Error::MissingElement("No sources provided".to_string()))?;

        debug!("Initialized {} sources", initialized_sources.len());

        let mut sources_mappings = SourcesMappings::new(initialized_sources)?;

        if let Some(ref mappings) = mappings {
            debug!("Applying {} custom track mappings", mappings.len());
            for &(source_idx, track_num) in mappings {
                sources_mappings.add_mapping(source_idx, track_num)?;
            }
        } else {
            debug!("Using default track mappings");
            if sources_mappings.add_first_video_track().is_err() {
                debug!("No video track found");
            }
            sources_mappings.add_all_audio_tracks()?;
            sources_mappings.add_all_subtitle_tracks()?;
        }

        let output_tracks = sources_mappings.get_output_tracks_metadata()?;
        debug!("Output will have {} tracks", output_tracks.track_entry.len());

        let chapters = sources_mappings
            .sources
            .iter()
            .find_map(|source| source.get_chapters().ok().flatten());
            

        let mut melting_pot = MeltingPot::new(sources_mappings);
        let duration_ns = melting_pot.get_final_duration().unwrap_or(0);
        output_interval.end_ns = Some(output_interval.start_ns.unwrap_or(0) + duration_ns);
        let track_count = output_tracks.track_entry.len();

        debug!("Initializing output sink");
        let output_sink = output_sink.initialize_simple(
            &output_tracks,
            duration_ns,
            target_timescale,
            chapters.as_ref()
        )?;

        Ok((
            Self {
                melting_pot,
                output_sink,
                blocks_processed: 0,
                clusters_written: 0,
                track_count,
                duration_ns,
            },
            output_interval,
        ))
    }

    /// Process one cluster.
    ///
    /// Returns `Ok(())` while there are more clusters to process.
    /// Returns `Err(Error::Done)` when all clusters have been written and the
    /// output has been finalized — the caller should treat this as normal
    /// termination, not a failure.
    pub fn process(mut self) -> Result<RemuxerState> {
        match self.melting_pot.generate_next_cluster()? {
            Some(cluster) => {
                self.blocks_processed += cluster.blocks.len() as u64;
                self.clusters_written += 1;
                if self.clusters_written % 100 == 0 {
                    debug!(
                        "Processed {} clusters, {} blocks",
                        self.clusters_written, self.blocks_processed
                    );
                }
                self.output_sink.write_cluster(&cluster, 0)?;
                Ok(RemuxerState::Processing(self))
            }
            None => {
                debug!("All clusters processed, finalizing output");
                let stats = self.stats();
                self.output_sink.finalize()?;
                debug!(
                    "Remux completed: {} clusters, {} blocks",
                    self.clusters_written, self.blocks_processed
                );
                Ok(RemuxerState::Done(stats))
            }
        }
    }

    /// Returns the current remux statistics.
    pub fn stats(&self) -> RemuxStats {
        RemuxStats {
            blocks_processed: self.blocks_processed,
            duration_ns: self.duration_ns,
            track_count: self.track_count,
        }
    }
}

/// Remux multiple input sources into a single output sink (convenience wrapper).
///
/// Internally creates a [`Remuxer`] and drives it to completion.
/// Remux multiple input sources into a single output sink (convenience wrapper).
///
/// Internally creates a [`Remuxer`] and drives it to completion.
pub fn remux(
    sources: Vec<InputSource<Uninitialized>>,
    output_sink: OutputSink<crate::sink::Uninitialized>,
    cut_interval: Option<CutInterval>,
    seek_type: Option<SeekType>,
    mappings: Option<Vec<TrackMapping>>,
) -> Result<RemuxStats> {
    let (mut remuxer, _actual_cut) =
        Remuxer::new(sources, output_sink, cut_interval, seek_type, mappings)?;

    loop {
        remuxer = match remuxer.process()? {
            RemuxerState::Processing(remuxer) => remuxer,
            RemuxerState::Done(stats) => return Ok(stats),
        }
    }
}

pub enum RemuxerState {
    Processing(Remuxer),
    Done(RemuxStats),
}