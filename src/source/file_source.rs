use super::{KeyframePositionCache, MkvReader, SeekType, Source};
use crate::block_ext::{ClusterBlockExt, ClusterExt, TrackKind, TracksExt};
use crate::source::CutInterval;
use crate::source::util::basic_info::MkvBasicInfo;
use crate::{Error, Result};
use clap::builder::Str;
use log::{debug, info, trace};
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct FileSource<T: MkvReader> {
    file: T,
    file_name: Option<String>,
    timecode_scale: u64,
    output_timecode_scale: u64,
    tracks: Tracks,
    info: Info,
    chapters: Option<Chapters>,
    cues: Option<Cues>,
    seek_type: SeekType,
    //input_cut_interval: CutInterval,
    output_interval: CutInterval,
    original_duration_ns: Option<u64>,
    /// position in the file where our first cluster of interest starts (usually around the specified cut start position)
    initial_cluster_pos: KeyframePositionCache,
    end_cluster_pos: Option<KeyframePositionCache>,
    finished: bool,
}

impl<T: MkvReader> FileSource<T> {
    pub fn new(mut reader: T) -> Result<Self> {
        // Read EBML header
        let ebml_header = Header::read_from(&mut reader)?;
        let _ebml = Ebml::read_element(&ebml_header, &mut reader)?;

        // Read Segment header
        let segment_header = Header::read_from(&mut reader)?;
        if segment_header.id.value != Segment::ID.value {
            return Err(Error::InvalidConfig(format!(
                "Expected Segment, found ID: {:x}",
                segment_header.id.value
            )));
        }

        // Scan for metadata
        let mut timecode_scale = 1_000_000u64;
        let mut tracks = None;
        let mut info = None;
        let mut chapters = None;
        let mut cues = None;
        let mut initial_cluster_pos: Option<KeyframePositionCache> = None;

        loop {
            let pos = reader.stream_position()?;
            let header = match Header::read_from(&mut reader) {
                Ok(h) => h,
                Err(e) => {
                    return Err(Error::InvalidFilePos(format!(
                        "header could not be loaded during init: {}",
                        e
                    )));
                }
            };

            if header.id == Info::ID {
                let info_elem = Info::read_element(&header, &mut reader)?;
                timecode_scale = info_elem.timestamp_scale.0;
                info = Some(info_elem);
            } else if header.id == Tracks::ID {
                tracks = Some(Tracks::read_element(&header, &mut reader)?);
            } else if header.id == Chapters::ID {
                chapters = Some(Chapters::read_element(&header, &mut reader)?);
            } else if header.id == Cues::ID {
                cues = Some(Cues::read_element(&header, &mut reader)?);
            } else if header.id == Cluster::ID {
                initial_cluster_pos = Some(KeyframePositionCache::form_file_pos(
                    reader.try_clone_reader()?,
                    timecode_scale,
                    0,
                    pos,
                )?);
                break;
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    reader.seek(SeekFrom::Current(size as i64))?;
                }
            }
        }

        debug!(
            "MkvRemuxer: We found {} cues in the file",
            cues.as_ref().map(|c| c.cue_point.len()).unwrap_or(0)
        );

        let initial_cluster_pos = initial_cluster_pos
            .ok_or_else(|| Error::InvalidConfig("No clusters found".to_string()))?;

        let original_duration_ns = info
            .as_ref()
            .and_then(|i| i.duration.map(|d| (d.0 * timecode_scale as f64) as u64));

        Ok(Self {
            file: reader,
            file_name: None,
            timecode_scale,
            output_timecode_scale: timecode_scale,
            tracks: tracks
                .ok_or_else(|| Error::InvalidConfig("Missing Tracks element".to_string()))?,
            info: info.ok_or_else(|| Error::InvalidConfig("Missing Info element".to_string()))?,
            chapters,
            cues,
            seek_type: SeekType::Squeeze, // default, can be changed in initialize_with_cut
            output_interval: CutInterval {
                start_ns: Some(0),
                end_ns: original_duration_ns,
            },
            original_duration_ns,
            initial_cluster_pos,
            end_cluster_pos: None,
            finished: false,
        })
    }

    pub fn with_file_name(mut self, file_name: String) -> Self {
        self.file_name = Some(file_name);
        self
    }

    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<FileSource<File>> {
        let file = File::open(path.as_ref())?;
        let file_name = path
            .as_ref()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(FileSource::new(file)?.with_file_name(file_name))
    }

    /// Find cluster position range from Cues for a given timestamp
    /// Returns (start_pos, end_pos) or None if Cues don't help narrow the range
    fn find_cluster_range_from_cues(&self, target_timestamp_ns: u64) -> Option<(u64, Option<u64>)> {
        let cues = self.cues.as_ref()?;

        // Convert target to timecode ticks
        let target_ticks = target_timestamp_ns / self.timecode_scale;

        // Find the cue point closest to but before the target
        let mut best_before: Option<&CuePoint> = None;
        let mut best_after: Option<&CuePoint> = None;

        for cue_point in &cues.cue_point {
            let cue_ticks = cue_point.cue_time.0;

            if cue_ticks <= target_ticks {
                if best_before.is_none() || cue_ticks > best_before.unwrap().cue_time.0 {
                    best_before = Some(cue_point);
                }
            } else {
                if best_after.is_none() || cue_ticks < best_after.unwrap().cue_time.0 {
                    best_after = Some(cue_point);
                }
            }
        }

        // Get file positions from cue track positions
        let start_pos = best_before?
            .cue_track_positions
            .first()?
            .cue_cluster_position
            .0;

        let end_pos = best_after
            .and_then(|cp| cp.cue_track_positions.first())
            .map(|ctp| ctp.cue_cluster_position.0);

        Some((start_pos, end_pos))
    }

    fn find_start_cluster(&mut self, target_timestamp_ns: u64) -> Result<()> {
        let file_len = self.file.stream_length()?;
        let range = match self.find_cluster_range_from_cues(target_timestamp_ns) {
            Some((start, end)) => {
                trace!(
                    "Using Cues to narrow start cluster search: start_pos={}, end_pos={:?}",
                    start, end
                );
                (start, end.unwrap_or(file_len))
            }
            None => {
                trace!("No Cues available, searching all clusters for start");
                (self.initial_cluster_pos.position, file_len)
            }
        };
        self.initial_cluster_pos = KeyframePositionCache::new(
            self.file.try_clone_reader()?,
            self.timecode_scale,
            target_timestamp_ns,
            Some(range),
        )?;
        Ok(())
    }

    fn find_end_cluster(&mut self, target_timestamp_ns: u64) -> Result<()> {
        let file_len = self.file.stream_length()?;
        let range = match self.find_cluster_range_from_cues(target_timestamp_ns) {
            Some((start, end)) => {
                trace!(
                    "Using Cues to narrow end cluster search: start_pos={}, end_pos={:?}",
                    start, end
                );
                (start, end.unwrap_or(file_len))
            }
            None => {
                trace!("No Cues available, searching all clusters for end");
                (self.initial_cluster_pos.position, file_len)
            }
        };
        self.end_cluster_pos = Some(KeyframePositionCache::new(
            self.file.try_clone_reader()?,
            self.timecode_scale,
            target_timestamp_ns,
            Some(range),
        )?);
        Ok(())
    }

    fn process_cluster_for_cut(&mut self, mut cluster: Cluster) -> Result<Cluster> {
        if self.initial_cluster_pos.get_timestamp_ns() == 0 && self.end_cluster_pos.is_none() {
            return Ok(cluster); // no cutting needed, just return original cluster
        }

        let orig_block_count = cluster.blocks.len();

        let orig_cluster_ticks = cluster.timestamp.0;
        let orig_cluster_ns = cluster.get_timestamp_ns(self.timecode_scale) as i64;

        // Shift cluster timestamp
        let shift_reference = match self.seek_type {
            SeekType::SnapNearestKeyframe(video_track_num) => self
                .initial_cluster_pos
                .get_closest_keyframe_timestamp_ns(video_track_num)?,
            SeekType::SnapPreviousKeyframe(video_track_num) => self
                .initial_cluster_pos
                .get_keyframe_timestamp_ns(video_track_num, false)?,
            SeekType::SnapNextKeyframe(video_track_num) => self
                .initial_cluster_pos
                .get_keyframe_timestamp_ns(video_track_num, true)?,
            _ => self.initial_cluster_pos.get_timestamp_ns(),
        };
        let actual_end_pos = match &mut self.end_cluster_pos {
            Some(end_pos) => match self.seek_type {
                SeekType::SnapNearestKeyframe(video_track_num) => {
                    Some(end_pos.get_closest_keyframe_timestamp_ns(video_track_num)?)
                }
                SeekType::SnapPreviousKeyframe(video_track_num) => {
                    Some(end_pos.get_keyframe_timestamp_ns(video_track_num, false)?)
                }
                SeekType::SnapNextKeyframe(video_track_num) => {
                    Some(end_pos.get_keyframe_timestamp_ns(video_track_num, true)?)
                }
                _ => Some(end_pos.get_timestamp_ns()),
            },
            None => None,
        };
        let shifted_ns = orig_cluster_ns - shift_reference as i64;
        cluster.timestamp.0 =
            ((shifted_ns as f64 / self.output_timecode_scale as f64).round()).max(0.0) as u64;
        let shifted_cluster_ticks = cluster.timestamp.0;

        let mut filtered = Vec::with_capacity(orig_block_count);

        let mut output_cluster: Cluster = match self.seek_type {
            SeekType::SnapNearestKeyframe(_)
            | SeekType::SnapPreviousKeyframe(_)
            | SeekType::SnapNextKeyframe(_) => {
                // Simple: just shift timestamps, but we still need to respect end_ns
                let mut filtered = Vec::with_capacity(cluster.blocks.len());
                for mut block in cluster.blocks {
                    let abs_ns =
                        block.timestamp_ns(orig_cluster_ticks as i64, self.timecode_scale)?;
                    if let Some(end) = actual_end_pos {
                        if abs_ns > end {
                            let tn = block.track_number()?;
                            trace!(
                                "dropping {:?} block (track {}) at {} ns: after cut end ({} ns)",
                                self.tracks.get_track_kind(tn),
                                tn,
                                abs_ns,
                                end
                            );
                            continue;
                        }
                    }

                    // only output after desired keyframe
                    if abs_ns < shift_reference {
                        let tn = block.track_number()?;
                        trace!(
                            "dropping {:?} block (track {}) at {} ns: before cut start/keyframe reference ({} ns)",
                            self.tracks.get_track_kind(tn),
                            tn,
                            abs_ns,
                            shift_reference
                        );
                        continue;
                    }
                    block.set_timestamp_ns(
                        (abs_ns - shift_reference).max(0) as u64,
                        cluster.timestamp.0,
                        self.output_timecode_scale,
                    )?;
                    let tn = block.track_number()?;
                    trace!(
                        "pushing {:?} block (track {}): ts_ns {}, abs_ns {}, end_ns {:?}, shift_ref {}",
                        self.tracks.get_track_kind(tn),
                        tn,
                        block
                            .timestamp_ns(cluster.timestamp.0 as i64, self.output_timecode_scale,)?,
                        abs_ns,
                        self.end_cluster_pos.as_ref().map(|e| e.get_timestamp_ns()),
                        shift_reference
                    );
                    filtered.push(block);
                }
                cluster.blocks = filtered;
                cluster
            }
            SeekType::DirtyCut => {
                // Drop frames outside range
                for mut block in cluster.blocks {
                    let abs_ns =
                        block.timestamp_ns(orig_cluster_ticks as i64, self.timecode_scale)?;
                    let start = self.initial_cluster_pos.get_timestamp_ns();
                    if abs_ns < start {
                        let tn = block.track_number()?;
                        trace!(
                            "dropping {:?} block (track {}) at {} ns: before cut start ({} ns)",
                            self.tracks.get_track_kind(tn),
                            tn,
                            abs_ns,
                            start
                        );
                        continue;
                    }
                    if let Some(end) = &self.end_cluster_pos {
                        let end = end.get_timestamp_ns();
                        if abs_ns > end {
                            let tn = block.track_number()?;
                            trace!(
                                "dropping {:?} block (track {}) at {} ns: after cut end ({} ns)",
                                self.tracks.get_track_kind(tn),
                                tn,
                                abs_ns,
                                end
                            );
                            continue;
                        }
                    }
                    let offset = abs_ns as i64 - start as i64;
                    block.set_timestamp_ns(
                        offset.max(0) as u64,
                        cluster.timestamp.0,
                        self.output_timecode_scale,
                    )?;
                    let tn = block.track_number()?;
                    trace!(
                        "pushing {:?} block (track {}): ts_ns {}, abs_ns {}, start_ns {}",
                        self.tracks.get_track_kind(tn),
                        tn,
                        block
                            .timestamp_ns(cluster.timestamp.0 as i64, self.output_timecode_scale,)?,
                        abs_ns,
                        start
                    );
                    filtered.push(block);
                }
                cluster.blocks = filtered;
                cluster
            }
            SeekType::Squeeze => {
                self.process_squeeze_cluster(cluster, orig_cluster_ticks, shifted_cluster_ticks)?
            }
        };

        if output_cluster.blocks.is_empty() && orig_block_count > 0 {
            debug!(
                "Cluster filtering removed all {} blocks (cluster_ns={}, start_ns={:?}, end_ns={:?})",
                orig_block_count,
                orig_cluster_ns,
                self.initial_cluster_pos.get_timestamp_ns(),
                self.end_cluster_pos.as_ref().map(|e| e.get_timestamp_ns())
            );
        }
        // NOTE: We do NOT sort here — the final sort happens in ClusterWriteWrapper::finish().
        // Sorting twice can cause subtle interleaving issues between audio/video at the
        // same tick, triggering audio cracks in Chrome's strict MSE demuxer.

        Ok(output_cluster)
    }

    fn process_squeeze_cluster(
        &mut self,
        mut cluster: Cluster,
        orig_cluster_ticks: u64,
        shifted_cluster_ticks: u64,
    ) -> Result<Cluster> {
        let mut filtered = Vec::with_capacity(cluster.blocks.len());
        for mut block in cluster.blocks {
            let track_num = block.track_number()?;
            let kind = self
                .tracks
                .get_track_kind(track_num)
                .ok_or_else(|| Error::TrackNotFound(track_num))?;

            let abs_ns = block
                .timestamp_ns(orig_cluster_ticks as i64, self.timecode_scale)
                .unwrap_or(0);

            match kind {
                TrackKind::Video => {
                    let start = self.initial_cluster_pos.get_timestamp_ns();
                    let last_keyframe_before_start_timestamp = self
                        .initial_cluster_pos
                        .get_keyframe_timestamp_ns(track_num, false)?;

                    // drop before last keyframe before start (before pre-roll is video-only)
                    if abs_ns < last_keyframe_before_start_timestamp as i64 {
                        trace!(
                            "dropping {:?} block (track {}) at {} ns: before last keyframe before cut start ({} ns)",
                            kind, track_num, abs_ns, last_keyframe_before_start_timestamp
                        );
                        continue;
                    }

                    // Drop after end
                    if let Some(end_pos) = &self.end_cluster_pos {
                        if abs_ns > end_pos.get_timestamp_ns() as i64 {
                            trace!(
                                "dropping {:?} block (track {}) at {} ns: after cut end ({} ns)",
                                kind,
                                track_num,
                                abs_ns,
                                end_pos.get_timestamp_ns()
                            );
                            continue;
                        }
                    }

                    // we are between last keyframe before start and start, aka pre-roll section,
                    if abs_ns < start as i64 {
                        if block.is_keyframe()?
                            && abs_ns > last_keyframe_before_start_timestamp as i64
                        {
                            let m = 1000_000_000.0;
                            return Err(Error::InternalBug(format!(
                                "Between last_keyframe_before_start_timestamp {} and start {} should be no more keyframes. found one at {}",
                                last_keyframe_before_start_timestamp as f64 / m,
                                start as f64 / m,
                                abs_ns as f64 / m
                            )));
                        }
                        // Pre-roll: squeeze to time 0 and mark invisible
                        block.set_timestamp_ns(
                            0,
                            shifted_cluster_ticks,
                            self.output_timecode_scale,
                        )?;
                        block.set_invisible(true)?;
                        trace!(
                            "pushing {:?} block (track {}): pre-roll squeezed to ts_ns 0 (abs_ns {}, was in pre-roll window before cut start {})",
                            kind, track_num, abs_ns, start
                        );
                    } else {
                        // No end: just shift by squeeze window
                        let offset = (abs_ns - start).max(0) as u64;
                        block.set_timestamp_ns(
                            offset,
                            shifted_cluster_ticks,
                            self.output_timecode_scale,
                        )?;
                        trace!(
                            "pushing {:?} block (track {}): ts_ns {}, abs_ns {}, start_ns {}",
                            kind, track_num, offset, abs_ns, start
                        );
                    }
                }
                _ => {
                    // Other tracks: just shift timestamps
                    // Drop after end
                    if let Some(end_pos) = &self.end_cluster_pos {
                        if abs_ns > end_pos.get_timestamp_ns() as i64 {
                            trace!(
                                "dropping {:?} block (track {}) at {} ns: after cut end ({} ns)",
                                kind,
                                track_num,
                                abs_ns,
                                end_pos.get_timestamp_ns()
                            );
                            continue;
                        }
                    }
                    let start = self.initial_cluster_pos.get_timestamp_ns();
                    {
                        // Drop  before start (pre-roll is video-only)
                        if abs_ns < start as i64 {
                            trace!(
                                "dropping {:?} block (track {}) at {} ns: before cut start ({} ns)",
                                kind, track_num, abs_ns, start
                            );
                            continue;
                        }
                        // Shift to start after squeeze window
                        let offset = (abs_ns - start).max(0) as u64;
                        block.set_timestamp_ns(
                            offset,
                            shifted_cluster_ticks,
                            self.output_timecode_scale,
                        )?;
                        trace!(
                            "pushing {:?} block (track {}): ts_ns {}, abs_ns {}, start_ns {}",
                            kind, track_num, offset, abs_ns, start
                        );
                    }
                }
            }

            filtered.push(block);
        }

        cluster.blocks = filtered;
        Ok(cluster)
    }
}

impl<T: MkvReader> fmt::Display for FileSource<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileSource",)
    }
}

impl<T: MkvReader> Source for FileSource<T> {
    fn get_tracks(&self) -> Result<Tracks> {
        Ok(self.tracks.clone())
    }

    fn get_chapters(&self) -> Result<Option<Chapters>> {
        Ok(self.chapters.clone())
    }

    fn get_info(&self) -> Result<Info> {
        Ok(self.info.clone())
    }

    fn get_basic_info(&self) -> Result<MkvBasicInfo> {
        let file_size = self.file.stream_length()?;
        Ok(MkvBasicInfo::new(
            &self.tracks,
            &self.info,
            file_size,
            self.file_name.clone().unwrap_or("unknown".to_string()),
        ))
    }

    fn get_output_interval(&mut self) -> Result<CutInterval> {
        Ok(self.output_interval.clone())
    }

    fn get_next_cluster(&mut self) -> Result<Option<Cluster>> {
        if self.finished {
            return Ok(None);
        }

        loop {
            let header = match Header::read_from(&mut self.file) {
                Ok(h) => h,
                Err(_) => {
                    self.finished = true;
                    return Ok(None);
                }
            };

            if header.id == Cluster::ID {
                let cluster = match Cluster::read_element(&header, &mut self.file) {
                    Ok(c) => c,
                    Err(_) => {
                        self.finished = true;
                        return Ok(None);
                    }
                };

                // Check if we should stop based on end time
                if let Some(end_pos) = &self.end_cluster_pos {
                    if cluster.get_timestamp_ns(self.timecode_scale) as i64
                        > end_pos.get_timestamp_ns()
                    {
                        trace!(
                            "Cluster at {} ns exceeds cut end {} ns, stopping",
                            cluster.get_timestamp_ns(self.timecode_scale),
                            end_pos.get_timestamp_ns()
                        );
                        self.finished = true;
                        return Ok(None);
                    }
                }

                let processed = self.process_cluster_for_cut(cluster)?;

                // Skip empty clusters (all blocks filtered out)
                if processed.blocks.is_empty() {
                    continue;
                }

                return Ok(Some(processed));
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    self.file.seek(SeekFrom::Current(size as i64))?;
                } else {
                    self.finished = true;
                    return Ok(None);
                }
            }
        }
    }

    fn get_own_timecode_scale(&self) -> Result<u64> {
        Ok(self.timecode_scale)
    }

    fn get_target_timecode_scale(&self) -> Result<u64> {
        Ok(self.output_timecode_scale)
    }

    fn initialize(&mut self, time_scale: Option<u64>) -> Result<CutInterval> {
        if let Some(ts) = time_scale {
            self.output_timecode_scale = ts;
        }

        self.file
            .seek(SeekFrom::Start(self.initial_cluster_pos.position))?;
        let duration = self
            .info
            .duration
            .map(|d| (d.0 * self.timecode_scale as f64) as u64);
        Ok(CutInterval {
            start_ns: Some(0),
            end_ns: duration,
        })
    }

    fn cut(&mut self, seek_type: SeekType, cut_interval: CutInterval) -> Result<CutInterval> {
        if let Some(start) = cut_interval.start_ns {
            self.find_start_cluster(start)?;
        }

        // Also find end cluster if end timestamp is provided
        if let Some(end) = cut_interval.end_ns {
            self.find_end_cluster(end)?;
        }

        self.seek_type = seek_type;

        // resolve the original cut position by filling in the duration and setting the start
        let cut_resolved = CutInterval {
            start_ns: Some(self.initial_cluster_pos.get_timestamp_ns().max(0) as u64),
            end_ns: self
                .end_cluster_pos
                .as_ref()
                .map(|end_pos| end_pos.get_timestamp_ns().max(0) as u64)
                .or_else(|| self.original_duration_ns),
        };

        let output_interval: CutInterval = match self.seek_type {
            SeekType::SnapNearestKeyframe(video_track_num) => {
                let actual_start_ns = self
                    .initial_cluster_pos
                    .get_closest_keyframe_timestamp_ns(video_track_num)?;
                // seek to cluster of the last keyframe to preserve all frames
                let keyframe_cluster_pos = self
                    .initial_cluster_pos
                    .get_keyframe_cluster_position(video_track_num, false)?;
                self.file.seek(SeekFrom::Start(keyframe_cluster_pos))?;
                let actual_end_ns = if let Some(end_pos) = &mut self.end_cluster_pos {
                    Some(end_pos.get_closest_keyframe_timestamp_ns(video_track_num)? as u64)
                } else {
                    self.original_duration_ns
                };
                CutInterval {
                    start_ns: Some(actual_start_ns as u64),
                    end_ns: actual_end_ns,
                }
            }
            SeekType::SnapPreviousKeyframe(video_track_num) => {
                let actual_start_ns = self
                    .initial_cluster_pos
                    .get_keyframe_timestamp_ns(video_track_num, false)?;
                // seek to cluster of the last keyframe to preserve all frames
                let keyframe_cluster_pos = self
                    .initial_cluster_pos
                    .get_keyframe_cluster_position(video_track_num, false)?;
                self.file.seek(SeekFrom::Start(keyframe_cluster_pos))?;
                let actual_end_ns = if let Some(end_pos) = &mut self.end_cluster_pos {
                    Some(end_pos.get_keyframe_timestamp_ns(video_track_num, false)? as u64)
                } else {
                    self.original_duration_ns
                };
                CutInterval {
                    start_ns: Some(actual_start_ns as u64),
                    end_ns: actual_end_ns,
                }
            }
            SeekType::SnapNextKeyframe(video_track_num) => {
                let actual_start_ns = self
                    .initial_cluster_pos
                    .get_keyframe_timestamp_ns(video_track_num, true)?;
                let actual_end_ns = if let Some(end_pos) = &mut self.end_cluster_pos {
                    Some(end_pos.get_keyframe_timestamp_ns(video_track_num, true)? as u64)
                } else {
                    self.original_duration_ns
                };
                CutInterval {
                    start_ns: Some(actual_start_ns as u64),
                    end_ns: actual_end_ns,
                }
            }
            SeekType::Squeeze => {
                let video_track_nums = self.tracks.get_all_video_tracks();
                trace!("Video track numbers: {:?}", video_track_nums);
                let mut earliest_keyframe_cluster_pos = self.initial_cluster_pos.position;
                for track_num in video_track_nums {
                    let keyframe_cluster_pos = self
                        .initial_cluster_pos
                        .get_keyframe_cluster_position(track_num, false)?;
                    //println!("Track {}, keyframe cluster position: {} original position: {}", track_num, keyframe_cluster_pos, self.initial_cluster_pos.position);
                    if keyframe_cluster_pos < earliest_keyframe_cluster_pos {
                        earliest_keyframe_cluster_pos = keyframe_cluster_pos;
                    }
                }
                // seek to the earliest keyframe cluster to preserve all frames, bc we dont know which track are picked we use the earliest cluster among all video tracks
                self.file
                    .seek(SeekFrom::Start(earliest_keyframe_cluster_pos))?;
                cut_resolved
            }
            SeekType::DirtyCut => cut_resolved,
        };
        self.output_interval = output_interval.clone();

        Ok(output_interval)
    }

    fn start_remuxing(&mut self) -> Result<()> {
        // dont seek here bc we already seeked to the correct position in initialize() or cut()
        Ok(())
    }
}
