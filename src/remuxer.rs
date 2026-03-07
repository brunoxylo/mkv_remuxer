use crate::block_ext::{TrackKind, TracksExt};
use crate::metling_pot::MeltingPot;
use crate::sink::{Initialized, OutputSink};
use crate::source::{self, CutInterval, Cutting, InputSource, Remuxing, SeekType, Uninitialized};
use crate::source_mappings::SourcesMappings;
use crate::{ContainerFormat, Error, Result};
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

// basically a mirrir is the SeekType you are no longer required to specify the video track number for the snap keyframe seek types
// the remuxer automatically determines according to the mapping which track to use for keyframe snapping
#[derive(Debug, Clone)]
pub enum RemuxerCutMode {
    Squeeze,
    SnapNearestKeyframe,
    SnapPreviousKeyframe,
    SnapNextKeyframe,
    DirtyCut,
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
    output_container: ContainerFormat, // most generous format mkv, then comes webm and last comes vtt
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
        cut_mode: Option<RemuxerCutMode>,
        mappings: Option<Vec<TrackMapping>>,
    ) -> Result<(Self, CutInterval)> {
        debug!("Initializing Remuxer with {} sources", sources.len());

        // initialize all sources fisrt
        // Derive the output timescale from the first source's own timescale.
        let mut target_timescale: Option<u64> = None;
        let mut sources_cutting: Vec<InputSource<Cutting>> = Vec::with_capacity(sources.len());
        for mut source in sources {
            match target_timescale {
                Some(ts) => { sources_cutting.push(source.initialize(Some(ts))?);
                },
                None => {
                    let source = source.initialize(None)?;
                    target_timescale = Some(source.get_target_timecode_scale()?);
                    sources_cutting.push(source);
                }
            }
        }

        let mut initialized_sources: Vec<InputSource<Remuxing>> = Vec::new();

        // cut  if requested, we want to know the output interval that might be different due to keyframe snap cut
        let mut output_cut_interval = if let Some(cut_interval) = cut_interval {
            match cut_mode {
                // search for the first video track to use as reference for snapping if needed, if no video track is found we just use dirty cut for all sources if any of the snap keyframe seek types is used
                Some(RemuxerCutMode::SnapNearestKeyframe) | Some(RemuxerCutMode::SnapPreviousKeyframe) | Some(RemuxerCutMode::SnapNextKeyframe) => {
                    let first_mapped_video_track = if let Some(ref mappings) = mappings { // there are custom mapping
                        let mut first_mapped = None;
                        for &(source_idx, track_num) in mappings {
                            if let Some(source) = sources_cutting.get(source_idx as usize) {
                                let tracks = source.get_tracks()?;
                                if let Some(track) = tracks.get_track_kind(track_num) {
                                    if track == TrackKind::Video {
                                        first_mapped = Some((source_idx, track_num));
                                        break;
                                    }
                                }
                            }
                        }
                        first_mapped
                    } else { // no custom mapping use first video track we can find
                        let mut first_mapped = None;
                        for (idx, source) in sources_cutting.iter().enumerate() {
                                let tracks = source.get_tracks()?;
                                if let Some(track) = tracks.track_entry.iter().find(|t| t.track_type.0 == TrackKind::Video) {
                                    first_mapped = Some((idx as u64, track.track_number.0));
                                    break;
                                }
                            }
                            first_mapped
                    };

                    // Cut the video source first to determine the snapped interval, then apply
                    // that interval to all other sources with DirtyCut — all sources stay in
                    // their original order so mapping indices remain valid.
                    let actual_cut = if let Some((video_src_idx, video_track_num)) = first_mapped_video_track {
                        let actual = match cut_mode {
                            Some(RemuxerCutMode::SnapNearestKeyframe) => sources_cutting[video_src_idx as usize].cut(SeekType::SnapNearestKeyframe(video_track_num), cut_interval)?,
                            Some(RemuxerCutMode::SnapPreviousKeyframe) => sources_cutting[video_src_idx as usize].cut(SeekType::SnapPreviousKeyframe(video_track_num), cut_interval)?,
                            Some(RemuxerCutMode::SnapNextKeyframe) => sources_cutting[video_src_idx as usize].cut(SeekType::SnapNextKeyframe(video_track_num), cut_interval)?,
                            _ => unreachable!(),
                        };
                        // Apply the snapped interval to all other sources with DirtyCut
                        for (idx, source) in sources_cutting.iter_mut().enumerate() {
                            if idx != video_src_idx as usize {
                                let _ = source.cut(SeekType::DirtyCut, actual)?;
                            }
                        }
                        actual
                    } else {
                        // No video track found; apply DirtyCut to all
                        for source in sources_cutting.iter_mut() {
                            let _ = source.cut(SeekType::DirtyCut, cut_interval)?;
                        }
                        cut_interval
                    };

                    // Push all sources in original order so mapping indices stay correct
                    for source in sources_cutting.into_iter() {
                        initialized_sources.push(source.into_remuxing()?);
                    }
                    actual_cut
                },
                Some(RemuxerCutMode::Squeeze) => {
                    for mut source in sources_cutting.into_iter() {
                        let _ = source.cut(SeekType::Squeeze, cut_interval)?;
                        initialized_sources.push(source.into_remuxing()?);
                    }
                    cut_interval
                },
                Some(RemuxerCutMode::DirtyCut) => {
                    for mut source in sources_cutting.into_iter() {
                        let _ = source.cut(SeekType::DirtyCut, cut_interval)?;
                        initialized_sources.push(source.into_remuxing()?);
                    }
                    cut_interval
                },
                None => { // cut interval requested but no cut mode specified, default to squeeze
                    for mut source in sources_cutting.into_iter() {
                        let _ = source.cut(SeekType::Squeeze, cut_interval)?;
                        initialized_sources.push(source.into_remuxing()?);
                    }
                    cut_interval
                }
            }
        } else { // no cut, just initialize remuxing directly
            let mut max_duration :Option<u64> = None;
            for mut source in sources_cutting.into_iter() {
                
                if let Some(dur) = source.get_output_duration()? {
                    max_duration = Some(dur.max(max_duration.unwrap_or(0)));
                }
                initialized_sources.push(source.into_remuxing()?);
            }
            CutInterval { start_ns: Some(0), end_ns: max_duration }
        };

        if initialized_sources.is_empty() {
            return Err(Error::InvalidConfig(
                "No valid sources after initialization".to_string(),
            ));
        }

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
        let duration_ns = melting_pot.get_final_duration()?.unwrap_or(0);
        output_cut_interval.end_ns = Some(output_cut_interval.start_ns.unwrap_or(0) + duration_ns);
        let track_count = output_tracks.track_entry.len();

        // determine which container fromat to use
        let output_format =if output_sink.does_support_container_format(ContainerFormat::Mkv) == true {
            if melting_pot.can_be_webm()? {
               ContainerFormat::WebM
            } else {
                ContainerFormat::Mkv
            }
        } else if output_sink.does_support_container_format(ContainerFormat::WebM) == true {
            if melting_pot.can_be_webm()? {
                ContainerFormat::WebM
            } else {
                return Err(Error::InvalidConfig(
                    "The provided sink does not support the required container format 'webm' for the given input sources. Please use a compatible sink or adjust the input sources to be compatible with webm.".to_string(),
                ));
            }
        } else if output_sink.does_support_container_format(ContainerFormat::Vtt) == true {
            if melting_pot.is_single_vtt_track()? {
                ContainerFormat::Vtt
            } else {
                return Err(Error::InvalidConfig(
                    "The provided sink does not support the required container format 'webvtt' for the given input sources. Please use a compatible sink or adjust the input sources to be compatible with webvtt.".to_string(),
                ));
            }
        } else {
            return Err(Error::InvalidConfig(
                "The provided sink does not support any of the container formats compatible with the given input sources. Please use a compatible sink or adjust the input sources.".to_string(),
            ));
        };

        debug!("MkvRemuxer: Container format: {:?}", output_format);

        debug!("Initializing output sink");
        let output_sink = output_sink.initialize_simple(
            &output_tracks,
            duration_ns,
            target_timescale,
            chapters.as_ref(),
            output_format,
        )?;

        Ok((
            Self {
                melting_pot,
                output_sink,
                blocks_processed: 0,
                clusters_written: 0,
                output_container: output_format,
                track_count,
                duration_ns,
            },
            output_cut_interval,
        ))
    }

    /// Returns the container format being used for the output.
    pub fn get_output_container_format(&self) -> ContainerFormat {
        self.output_container
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
    cut_mode: Option<RemuxerCutMode>,
    mappings: Option<Vec<TrackMapping>>,
) -> Result<RemuxStats> {
    let (mut remuxer, _actual_cut) =
        Remuxer::new(sources, output_sink, cut_interval, cut_mode, mappings)?;

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