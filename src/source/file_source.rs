use super::{KeyframePositionCache, SeekType, Source};
use crate::block_ext::{ClusterBlockExt, ClusterExt, TrackKind, TracksExt};
use crate::source::CutInterval;
use crate::source::util::basic_info::MkvBasicInfo;
use crate::{Error, Result};
use core::time;
use log::{debug, info, trace};
use mkv_element::io::blocking_impl::*;
use mkv_element::{ClusterBlock, prelude::*};
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;


pub struct FileSource {
    file: File,
    path: String,
    timecode_scale: u64,
    output_timecode_scale: u64,
    tracks: Tracks,
    info: Info,
    chapters: Option<Chapters>,
    cues: Option<Cues>,
    seek_type: SeekType,
    input_cut_interval: CutInterval,
    output_interval: CutInterval,
    original_duration_ns: Option<u64>,
    /// position in the file where our first cluster of interest starts (usually around the specified cut start position)
    initial_cluster_pos: KeyframePositionCache,
    end_cluster_pos: KeyframePositionCache,
    finished: bool,
}

impl FileSource {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let mut file = File::open(path)?;

        // Read EBML header
        let ebml_header = Header::read_from(&mut file)?;
        let _ebml = Ebml::read_element(&ebml_header, &mut file)?;

        // Read Segment header
        let segment_header = Header::read_from(&mut file)?;
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
            let pos = file.stream_position()?;
            let header = match Header::read_from(&mut file) {
                Ok(h) => h,
                Err(e) => {
                    return Err(Error::MkvElement(e));
                }
            };

            if header.id == Info::ID {
                let info_elem = Info::read_element(&header, &mut file)?;
                timecode_scale = info_elem.timestamp_scale.0;
                info = Some(info_elem);
            } else if header.id == Tracks::ID {
                tracks = Some(Tracks::read_element(&header, &mut file)?);
            } else if header.id == Chapters::ID {
                chapters = Some(Chapters::read_element(&header, &mut file)?);
            } else if header.id == Cues::ID {
                cues = Some(Cues::read_element(&header, &mut file)?);
            } else if header.id == Cluster::ID {
                initial_cluster_pos = Some(KeyframePositionCache::new(
                    pos,
                    file.try_clone()?,
                    timecode_scale,
                ));
                break;
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    file.seek(SeekFrom::Current(size as i64))?;
                }
            }
        }

        let initial_cluster_pos = initial_cluster_pos
            .ok_or_else(|| Error::InvalidConfig("No clusters found".to_string()))?;

        // Initialize end_cluster_pos at the same position as initial, will be updated during initialize_with_cut
        let end_cluster_pos = KeyframePositionCache::new(
            initial_cluster_pos.position,
            file.try_clone()?,
            timecode_scale,
        );

        let original_duration_ns = info
            .as_ref()
            .and_then(|i| i.duration.map(|d| (d.0 * timecode_scale as f64) as u64));

        Ok(Self {
            file,
            path: path_str,
            timecode_scale,
            output_timecode_scale: timecode_scale,
            tracks: tracks
                .ok_or_else(|| Error::InvalidConfig("Missing Tracks element".to_string()))?,
            info: info.ok_or_else(|| Error::InvalidConfig("Missing Info element".to_string()))?,
            chapters,
            cues,
            seek_type: SeekType::Squeeze, // default, can be changed in initialize_with_cut
            input_cut_interval: CutInterval::new(), // default, can be changed in initialize_with
            output_interval: CutInterval {
                start_ns: Some(0),
                end_ns: original_duration_ns,
            },
            original_duration_ns,
            initial_cluster_pos,
            end_cluster_pos,
            finished: false,
        })
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

    /// Build an index of cluster positions and timestamps for binary search
    /// If start_pos and end_pos are provided, only index clusters in that range
    fn build_cluster_index(&mut self, start_pos: Option<u64>, end_pos: Option<u64>) -> Result<Vec<(u64, u64)>> {
        // Returns Vec of (file_position, timestamp_ns)
        let mut cluster_index = Vec::new();
        
        let start = start_pos.unwrap_or(self.initial_cluster_pos.position);
        self.file.seek(SeekFrom::Start(start))?;
        
        loop {
            let current_pos = self.file.stream_position()?;
            
            // Stop if we've reached the end position
            if let Some(end) = end_pos {
                if current_pos >= end {
                    break;
                }
            }
            
            let header = match Header::read_from(&mut self.file) {
                Ok(h) => h,
                Err(_) => break,
            };
            
            if header.id == Cluster::ID {
                let cluster = match Cluster::read_element(&header, &mut self.file) {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let timestamp_ns = cluster.get_timestamp_ms(self.timecode_scale);
                cluster_index.push((current_pos, timestamp_ns));
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    self.file.seek(SeekFrom::Current(size as i64))?;
                } else {
                    break;
                }
            }
        }
        
        Ok(cluster_index)
    }

    /// Find cluster position using binary search on cluster index
    fn find_cluster_by_binary_search(
        &mut self,
        target_timestamp_ns: u64,
        is_start: bool,
    ) -> Result<u64> {
        // Try to narrow the search range using Cues if available
        let (start_pos, end_pos) = if let Some((start, end)) = self.find_cluster_range_from_cues(target_timestamp_ns) {
            trace!("Using Cues to narrow cluster search: start_pos={}, end_pos={:?}", start, end);
            (Some(start), end)
        } else {
            trace!("No Cues available, scanning all clusters");
            (None, None)
        };
        
        let cluster_index = self.build_cluster_index(start_pos, end_pos)?;
        
        if cluster_index.is_empty() {
            return Err(Error::InvalidConfig("No clusters found".to_string()));
        }
        
        let video_track_numbers = self.tracks.get_all_video_tracks();
        let video_num = video_track_numbers.first();
        
        // Binary search for the target timestamp
        let result = cluster_index.binary_search_by_key(&target_timestamp_ns, |(_, ts)| *ts);
        
        let cluster_idx = match result {
            Ok(idx) => idx, // Exact match
            Err(idx) => {
                // idx is where target would be inserted
                if idx == 0 {
                    0 // Target is before first cluster
                } else if is_start {
                    idx - 1 // For start, use cluster before target
                } else {
                    idx.min(cluster_index.len() - 1) // For end, use cluster at or after target
                }
            }
        };
        
        // If we have video tracks, find the nearest cluster with a keyframe
        if let Some(video_track_num) = video_num {
            // Search backward from found position to find cluster with keyframe
            for i in (0..=cluster_idx).rev() {
                let (pos, _) = cluster_index[i];
                self.file.seek(SeekFrom::Start(pos))?;
                let header = Header::read_from(&mut self.file)?;
                if header.id == Cluster::ID {
                    let cluster = Cluster::read_element(&header, &mut self.file)?;
                    if cluster.has_keyframes(*video_track_num) {
                        return Ok(pos);
                    }
                }
            }
            // If no keyframe found backward, search forward
            for i in cluster_idx..cluster_index.len() {
                let (pos, _) = cluster_index[i];
                self.file.seek(SeekFrom::Start(pos))?;
                let header = Header::read_from(&mut self.file)?;
                if header.id == Cluster::ID {
                    let cluster = Cluster::read_element(&header, &mut self.file)?;
                    if cluster.has_keyframes(*video_track_num) {
                        return Ok(pos);
                    }
                }
            }
        }
        
        // No video track or no keyframe requirement, use found position
        Ok(cluster_index[cluster_idx].0)
    }

    fn find_start_cluster(&mut self, target_timestamp_ns: u64) -> Result<()> {
        let pos = self.find_cluster_by_binary_search(target_timestamp_ns, true)?;
        self.initial_cluster_pos.set_pos(pos);
        Ok(())
    }
    
    fn find_end_cluster(&mut self, target_timestamp_ns: u64) -> Result<()> {
        let pos = self.find_cluster_by_binary_search(target_timestamp_ns, false)?;
        self.end_cluster_pos.set_pos(pos);
        Ok(())
    }

    fn process_cluster_for_cut(&mut self, mut cluster: Cluster) -> Result<Cluster> {
        if self.input_cut_interval.start_ns.is_none() && self.input_cut_interval.end_ns.is_none() {
            return Ok(cluster); // no cutting needed, just return original cluster
        }

        let orig_block_count = cluster.blocks.len();

        let orig_cluster_ticks = cluster.timestamp.0 as i64;
        let orig_cluster_ns = cluster.get_timestamp_ms(self.timecode_scale) as i64;

        // Shift cluster timestamp
        let shift_reference = match self.seek_type {
            SeekType::SnapNearestKeyframe(video_track_num) => {
                    self.initial_cluster_pos
                        .get_closest_keyframe_timestamp_ns(video_track_num, self.input_cut_interval.start_ns.unwrap_or(0) as i64)?
            }
            SeekType::SnapPreviousKeyframe(video_track_num) => {
                self.initial_cluster_pos
                    .get_keyframe_timestamp_ns(video_track_num, self.input_cut_interval.start_ns.unwrap_or(0) as i64, false)?
            }
            SeekType::SnapNextKeyframe(video_track_num) => {
                self.initial_cluster_pos
                    .get_keyframe_timestamp_ns(video_track_num, self.input_cut_interval.start_ns.unwrap_or(0) as i64, true)?
            }
            _ => {
                self.input_cut_interval.start_ns.unwrap_or(0) as i64
            }
        };
        let shifted_ns = orig_cluster_ns - shift_reference as i64;
        cluster.timestamp.0 = (shifted_ns / self.output_timecode_scale as i64).max(0) as u64;
        let shifted_cluster_ticks = cluster.timestamp.0 as i64;

        let mut filtered = Vec::with_capacity(orig_block_count);

        let result = match self.seek_type {
            SeekType::SnapNearestKeyframe(_) | SeekType::SnapPreviousKeyframe(_) | SeekType::SnapNextKeyframe(_) => {
                // Simple: just shift timestamps, but we still need to respect end_ns
                let mut filtered = Vec::with_capacity(cluster.blocks.len());
                for mut block in cluster.blocks {
                    let abs_ns = block.timestamp_ns(orig_cluster_ticks, self.timecode_scale)?;
                    if let Some(end) = self.input_cut_interval.end_ns {
                        if abs_ns as u64 > end {
                            //print!("Block at {} ns is after cut end {} ns, dropping", abs_ns, end);
                            continue;
                        }
                    } 
                    
                    // only output after desired keyframe
                    if abs_ns < shift_reference {
                        continue;
                    }
                    block.set_timestamp_ns(
                        abs_ns - shift_reference,
                        cluster.timestamp.0 as i64,
                        self.output_timecode_scale,
                    )?;
                    trace!("pushing block with: {}, abs_ns {}, end_ns {:?}, shift_ref {}", block.timestamp_ns(cluster.timestamp.0 as i64, self.output_timecode_scale,)?, abs_ns, self.input_cut_interval.end_ns, shift_reference);
                    filtered.push(block);
                }
                cluster.blocks = filtered;
                Ok(cluster)
            }
            SeekType::DirtyCut => {
                // Drop frames outside range
                for mut block in cluster.blocks {
                    let abs_ns = block.timestamp_ns(orig_cluster_ticks, self.timecode_scale)?;
                    let abs_ns = abs_ns as u64;
                    if let Some(start) = self.input_cut_interval.start_ns {
                        if abs_ns < start {
                            continue;
                        }
                    }
                    if let Some(end) = self.input_cut_interval.end_ns {
                        if abs_ns > end {
                            continue;
                        }
                    }
                    if let Some(start) = self.input_cut_interval.start_ns {
                        let offset = abs_ns as i64 - start as i64;
                        block.set_timestamp_ns(
                            offset,
                            cluster.timestamp.0 as i64,
                            self.output_timecode_scale,
                        )?;
                    }
                    filtered.push(block);
                }
                cluster.blocks = filtered;
                Ok(cluster)
            }
            SeekType::Squeeze => {
                self.process_squeeze_cluster(cluster, orig_cluster_ticks, shifted_cluster_ticks)
            }
        };

        if let Ok(ref processed) = result {
            if processed.blocks.is_empty() && orig_block_count > 0 {
                debug!(
                    "Cluster filtering removed all {} blocks (cluster_ns={}, start_ns={:?}, end_ns={:?})",
                    orig_block_count,
                    orig_cluster_ns,
                    self.input_cut_interval.start_ns,
                    self.input_cut_interval.end_ns
                );
            }
        }

        result
    }

    fn process_squeeze_cluster(
        &self,
        mut cluster: Cluster,
        orig_cluster_ticks: i64,
        shifted_cluster_ticks: i64,
    ) -> Result<Cluster> {
        let mut filtered = Vec::with_capacity(cluster.blocks.len());
        for mut block in cluster.blocks {
            let track_num = block.track_number()?;
            let kind = self
                .tracks
                .get_track_kind(track_num)
                .ok_or_else(|| Error::TrackNotFound(track_num))?;

            let abs_ns = block
                .timestamp_ns(orig_cluster_ticks, self.timecode_scale)
                .unwrap_or(0);

            match kind {
                TrackKind::Video => {
                    // Drop after end
                    if let Some(end) = self.input_cut_interval.end_ns {
                        if abs_ns > end as i64 {
                            continue;
                        }
                    }
                    if let Some(start) = self.input_cut_interval.start_ns {
                        if abs_ns < start as i64 {
                            // Pre-roll: squeeze to time 0 and mark invisible
                            block.set_timestamp_ns(
                                0,
                                shifted_cluster_ticks,
                                self.output_timecode_scale,
                            )?;
                            block.set_invisible(true)?;
                            trace!("Set block duration to 0 for pre-roll block at {} ns", abs_ns);
                        } else if let Some(end) = self.input_cut_interval.end_ns {
                            if abs_ns > end as i64 {
                                continue; // Drop post-roll for now (could squeeze at end)
                            } else {
                                // Main content: shift by squeeze window
                                let offset = abs_ns - start as i64;
                                block.set_timestamp_ns(
                                    offset,
                                    shifted_cluster_ticks,
                                    self.output_timecode_scale,
                                )?;
                            }
                        } else {
                            // No end: just shift by squeeze window
                            let offset = abs_ns - start as i64;
                            block.set_timestamp_ns(
                                offset,
                                shifted_cluster_ticks,
                                self.output_timecode_scale,
                            )?;
                        }
                    }
                }
                _ => {
                    // Other tracks: just shift timestamps
                    // Drop after end
                    if let Some(end) = self.input_cut_interval.end_ns {
                        if abs_ns > end as i64 {
                            continue;
                        }
                    }
                    if let Some(start) = self.input_cut_interval.start_ns {
                        // Drop  before start (pre-roll is video-only)
                        if abs_ns < start as i64 {
                            continue;
                        }
                        // Shift to start after squeeze window
                        let offset = abs_ns - start as i64;
                        block.set_timestamp_ns(
                            offset,
                            shifted_cluster_ticks,
                            self.output_timecode_scale,
                        )?;
                    }
                }
            }

            filtered.push(block);
        }

        cluster.blocks = filtered;
        Ok(cluster)
    }
}

impl fmt::Display for FileSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileSource",)
    }
}

impl Source for FileSource {
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
        let file_size = self.file.metadata()?.len();
        let file_name = std::path::Path::new(&self.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.clone());
        Ok(MkvBasicInfo::new(&self.tracks, &self.info, file_size, file_name))
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
                if let Some(end_ns) = self.input_cut_interval.end_ns {
                    if cluster.get_timestamp_ms(self.timecode_scale) > end_ns {
                        trace!("Cluster at {} ns exceeds cut end {} ns, stopping", cluster.get_timestamp_ms(self.timecode_scale), end_ns);
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
        let duration = self.info.duration.map(|d| (d.0 * self.timecode_scale as f64) as u64);
        Ok(CutInterval {
            start_ns: Some(0),
            end_ns: duration,
        })
    }

    fn cut(
        &mut self,
        seek_type: SeekType,
        cut_interval: CutInterval,
    ) -> Result<CutInterval> {

        if let Some(start) = cut_interval.start_ns {
            self.find_start_cluster(start)?;
        }

        // Also find end cluster if end timestamp is provided
        if let Some(end) = cut_interval.end_ns {
            self.find_end_cluster(end)?;
        }

        self.input_cut_interval = cut_interval;
        self.seek_type = seek_type;

        let output_interval: CutInterval = match self.seek_type {
            SeekType::SnapNearestKeyframe(video_track_num) => {
                let actual_start_ns = self.initial_cluster_pos.get_closest_keyframe_timestamp_ns(
                    video_track_num,
                    self.input_cut_interval.start_ns.unwrap_or(0) as i64,
                )?;
                let actual_end_ns = if let Some(end_ns) = self.input_cut_interval.end_ns {
                    Some(
                        self.end_cluster_pos.get_closest_keyframe_timestamp_ns(
                            video_track_num,
                            end_ns as i64,
                        )? as u64
                    )
                } else {
                    self.original_duration_ns
                };
                CutInterval {
                    start_ns: Some(actual_start_ns as u64),
                    end_ns: actual_end_ns,
                }
            }
            SeekType::SnapPreviousKeyframe(video_track_num) => {
                let actual_start_ns = self.initial_cluster_pos.get_keyframe_timestamp_ns(
                    video_track_num,
                    self.input_cut_interval.start_ns.unwrap_or(0) as i64,
                    false,
                )?;
                let actual_end_ns = if let Some(end_ns) = self.input_cut_interval.end_ns {
                    Some(
                        self.end_cluster_pos.get_keyframe_timestamp_ns(
                            video_track_num,
                            end_ns as i64,
                            false,
                        )? as u64
                    )
                } else {
                    self.original_duration_ns
                };
                CutInterval {
                    start_ns: Some(actual_start_ns as u64),
                    end_ns: actual_end_ns,
                }
            }
            SeekType::SnapNextKeyframe(video_track_num) => {
                let actual_start_ns = self.initial_cluster_pos.get_keyframe_timestamp_ns(
                    video_track_num,
                    self.input_cut_interval.start_ns.unwrap_or(0) as i64,
                    true,
                )?;
                let actual_end_ns = if let Some(end_ns) = self.input_cut_interval.end_ns {
                    Some(
                        self.end_cluster_pos.get_keyframe_timestamp_ns(
                            video_track_num,
                            end_ns as i64,
                            true,
                        )? as u64
                    )
                } else {
                    self.original_duration_ns
                };
                CutInterval {
                    start_ns: Some(actual_start_ns as u64),
                    end_ns: actual_end_ns,
                }
            }
            SeekType::DirtyCut | SeekType::Squeeze => {
                CutInterval {
                    start_ns: self.input_cut_interval.start_ns.or(Some(0)),
                    end_ns: self.input_cut_interval.end_ns.or_else(|| self.original_duration_ns),
                }
            }
        };
        self.output_interval = output_interval.clone();

        Ok(output_interval)
    }

    fn start_remuxing(&mut self) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(self.initial_cluster_pos.position))?;
        Ok(())
    }
}
