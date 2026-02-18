use super::{ClusterOfInterestCache, SeekType, Source};
use crate::block_ext::{ClusterBlockExt, ClusterExt, TrackKind, TracksExt};
use crate::{Error, Result};
use core::time;
use log::{debug, info};
use mkv_element::io::blocking_impl::*;
use mkv_element::{ClusterBlock, prelude::*};
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use super::CutConfig;


pub struct FileSource {
    file: File,
    timecode_scale: u64,
    output_timecode_scale: u64,
    tracks: Tracks,
    info: Info,
    chapters: Option<Chapters>,
    cut_parameters: CutConfig,
    /// position in the file where our first cluster of interest starts (usually around the specified cut start position)
    initial_cluster_pos: ClusterOfInterestCache,
    end_cluster_pos: ClusterOfInterestCache,
    first_video_track_num: Option<u64>,
    finished: bool,
}

impl FileSource {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
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
        let mut initial_cluster_pos: Option<ClusterOfInterestCache> = None;

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
            } else if header.id == Cluster::ID {
                initial_cluster_pos = Some(ClusterOfInterestCache::new(
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

        let first_video_track_num = tracks
            .as_ref()
            .ok_or_else(|| Error::InvalidConfig("Missing Tracks element".to_string()))?
            .get_all_video_tracks()
            .first()
            .cloned();

        // Initialize end_cluster_pos at the same position as initial, will be updated during initialize_with_cut
        let end_cluster_pos = ClusterOfInterestCache::new(
            initial_cluster_pos.position,
            file.try_clone()?,
            timecode_scale,
        );

        Ok(Self {
            file,
            timecode_scale,
            output_timecode_scale: timecode_scale,
            tracks: tracks
                .ok_or_else(|| Error::InvalidConfig("Missing Tracks element".to_string()))?,
            info: info.ok_or_else(|| Error::InvalidConfig("Missing Info element".to_string()))?,
            chapters,
            cut_parameters: CutConfig {
                seek_type: SeekType::SnapNearestKeyframe,
                start_ns: None,
                end_ns: None,
            },
            initial_cluster_pos,
            end_cluster_pos,
            finished: false,
            first_video_track_num,
        })
    }

    /// Build an index of all cluster positions and timestamps for binary search
    fn build_cluster_index(&mut self) -> Result<Vec<(u64, u64)>> {
        // Returns Vec of (file_position, timestamp_ns)
        let mut cluster_index = Vec::new();
        
        self.file.seek(SeekFrom::Start(self.initial_cluster_pos.position))?;
        
        loop {
            let current_pos = self.file.stream_position()?;
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
        let cluster_index = self.build_cluster_index()?;
        
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
        if self.cut_parameters.start_ns.is_none() && self.cut_parameters.end_ns.is_none() {
            return Ok(cluster); // no cutting needed, just return original cluster
        }

        let orig_block_count = cluster.blocks.len();

        let orig_cluster_ticks = cluster.timestamp.0 as i64;
        let orig_cluster_ns = cluster.get_timestamp_ms(self.timecode_scale) as i64;

        // Shift cluster timestamp
        let shift_reference =  if self.cut_parameters.seek_type == SeekType::SnapNearestKeyframe {
            if let Some(video_track_num) = self.first_video_track_num {
                self.initial_cluster_pos
                    .get_closest_keyframe_timestamp_ns(video_track_num, self.cut_parameters.start_ns.unwrap_or(0) as i64)?
            } else {
                // no video tracks, use cut start as reference for shifting 
                self.cut_parameters.start_ns.unwrap_or(0) as i64
            }
        } else {
            self.cut_parameters.start_ns.unwrap_or(0) as i64
        };
        let shifted_ns = orig_cluster_ns - shift_reference as i64;
        cluster.timestamp.0 = (shifted_ns / self.output_timecode_scale as i64).max(0) as u64;
        let shifted_cluster_ticks = cluster.timestamp.0 as i64;

        let mut filtered = Vec::with_capacity(orig_block_count);

        let result = match self.cut_parameters.seek_type {
            SeekType::SnapNearestKeyframe => {
                // Simple: just shift timestamps, but we still need to respect end_ns
                let mut filtered = Vec::with_capacity(cluster.blocks.len());
                for mut block in cluster.blocks {
                    let abs_ns = block.timestamp_ns(orig_cluster_ticks, self.timecode_scale)?;
                    if let Some(end) = self.cut_parameters.end_ns {
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
                    print!("pushing block with: {}, abs_ns {}, end_ns {:?}, shift_ref {}", block.timestamp_ns(cluster.timestamp.0 as i64, self.output_timecode_scale,)?, abs_ns, self.cut_parameters.end_ns, shift_reference);
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
                    if let Some(start) = self.cut_parameters.start_ns {
                        if abs_ns < start {
                            continue;
                        }
                    }
                    if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end {
                            continue;
                        }
                    }
                    if let Some(start) = self.cut_parameters.start_ns {
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
            SeekType::Freeze => {
                self.process_freeze_cluster(cluster, orig_cluster_ticks, shifted_cluster_ticks)
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
                    self.cut_parameters.start_ns,
                    self.cut_parameters.end_ns
                );
            }
        }

        result
    }

    fn process_freeze_cluster(
        &mut self,
        mut cluster: Cluster,
        orig_cluster_ticks: i64,
        shifted_cluster_ticks: i64,
    ) -> Result<Cluster> {
        let mut is_freeze_frame_set = false;
        let mut filtered = Vec::with_capacity(cluster.blocks.len());
        for i in 0..cluster.blocks.len() {
            let track_num = cluster.blocks[i].track_number()?;
            let kind = self
                .tracks
                .get_track_kind(track_num)
                .ok_or_else(|| Error::TrackNotFound(track_num))?;

            let abs_ns = cluster.blocks[i]
                .timestamp_ns(orig_cluster_ticks, self.timecode_scale)
                .unwrap_or(0);

            match kind {
                TrackKind::Video => {
                    // Drop video after end
                    if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end as i64 {
                            continue;
                        }
                    }

                    if let Some(start) = self.cut_parameters.start_ns {
                        let start = start as i64;
                        if abs_ns < start {
                            continue;
                        } // Drop non-keyframes before start

                        let keyframe_timestamp_ns = self
                            .initial_cluster_pos
                            .get_keyframe_timestamp_ns(track_num, start, true)?;

                        if abs_ns >= start && abs_ns <= keyframe_timestamp_ns {
                            debug!("inside freeze zone");

                            if !is_freeze_frame_set {
                                let cluster_pos = self
                                    .initial_cluster_pos
                                    .get_keyframe_block_idx(track_num, true, start)?;
                                if let Some(keyframe_block) = cluster.blocks.get(cluster_pos) {
                                    let mut new_block = keyframe_block.clone(); // use keyframe block as freeze frame
                                    new_block.set_timestamp_ns(
                                        0,
                                        cluster.timestamp.0 as i64,
                                        self.output_timecode_scale,
                                    )?;
                                    filtered.push(new_block);
                                    is_freeze_frame_set = true;
                                } else {
                                    debug!("No keyframe block found for freeze frame");
                                }
                            } else {
                                continue; // Drop non-keyframes before first keyframe
                            }
                        } else {
                            // Shift video
                            let offset = abs_ns as i64 - start as i64;
                            let mut block = cluster.blocks[i].clone();
                            block.set_timestamp_ns(
                                offset,
                                cluster.timestamp.0 as i64,
                                self.output_timecode_scale,
                            )?;
                            filtered.push(block);
                        }
                    }
                }
                _ => {
                    // all other tracks just shift timestamps if within range
                    // Drop after end
                    if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end as i64 {
                            continue;
                        }
                    }
                    // Drop before start
                    if let Some(start) = self.cut_parameters.start_ns {
                        if abs_ns < start as i64 {
                            continue;
                        }
                        // Shift audio
                        let offset = abs_ns as i64 - start as i64;
                        let mut block = cluster.blocks[i].clone();
                        block.set_timestamp_ns(
                            offset,
                            cluster.timestamp.0 as i64,
                            self.output_timecode_scale,
                        )?;
                        filtered.push(block);
                    } else { // just add to output if no start specified
                        filtered.push(cluster.blocks[i].clone());
                    }
                }
            }
        }

        cluster.blocks = filtered;
        Ok(cluster)
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
                    if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end as i64 {
                            continue;
                        }
                    }
                    if let Some(start) = self.cut_parameters.start_ns {
                        if abs_ns < start as i64 {
                            // Pre-roll: squeeze to time 0 and mark invisible
                            block.set_timestamp_ns(
                                0,
                                shifted_cluster_ticks,
                                self.output_timecode_scale,
                            )?;
                            block.set_invisible(true)?;
                        } else if let Some(end) = self.cut_parameters.end_ns {
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
                    if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end as i64 {
                            continue;
                        }
                    }
                    if let Some(start) = self.cut_parameters.start_ns {
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

    fn get_cut_positions(&self) -> (u64, Option<u64>) {
        (
            self.cut_parameters.start_ns.unwrap_or(0),
            self.cut_parameters.end_ns,
        )
    }

    fn get_duration(& mut self) -> Result<u64> {
        let orig_duration =     match self.info.duration {
                Some(duration) => Some((duration.0 * self.timecode_scale as f64) as u64),
                None => None,
        };
        let video_track_num = self.first_video_track_num;

        if let Some(v_track) = video_track_num && self.cut_parameters.seek_type == SeekType::SnapNearestKeyframe {
            let start_keyframe = self.initial_cluster_pos.get_closest_keyframe_timestamp_ns(
                v_track,
                self.cut_parameters.start_ns.unwrap_or(0) as i64,
            )?;
            if let Some (end_pos) = self.cut_parameters.end_ns {
                let end_keyframe = self.initial_cluster_pos.get_closest_keyframe_timestamp_ns(
                    v_track,
                    end_pos as i64,
                )?;
                if end_keyframe > start_keyframe {
                    return Ok((end_keyframe - start_keyframe) as u64);
                } else {
                    return Err(Error::InvalidConfig("End keyframe is before start keyframe".to_string()));
                }
            } else {
                if let Some(orig_dur) = orig_duration {
                    if orig_dur > start_keyframe as u64 {
                        return Ok(orig_dur - start_keyframe as u64);
                    } else {
                        return Err(Error::InvalidConfig("start keyframe exceeds original duration".to_string()));
                    }
                } else {
                    return Err(Error::InvalidConfig("Original duration unknown, cannot calculate cut duration without end timestamp".to_string()));
                }
            }
        } else {
            // no video tracks or no SnapKeyframe, just use cut start and end timestamps to calculate duration
            let start = self.cut_parameters.start_ns.unwrap_or(0);
           if let Some(dur) = orig_duration {
                let end = self.cut_parameters.end_ns.unwrap_or(dur);
                if end > start {
                    return Ok(end - start);
                } else {
                    return Err(Error::InvalidConfig("End timestamp is before start timestamp".to_string()));
                }
            } else {
                if let Some(end) = self.cut_parameters.end_ns {
                    if end > start {
                        return Ok(end - start);
                    } else {
                        return Err(Error::InvalidConfig("End timestamp is before start timestamp".to_string()));
                    }
                } else {
                    return Err(Error::InvalidConfig("Original duration unknown, cannot calculate cut duration without end timestamp".to_string()));
                }
            }
        }

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
                if let Some(end_ns) = self.cut_parameters.end_ns {
                    if cluster.get_timestamp_ms(self.timecode_scale) > end_ns {
                        println!("Cluster at {} ns exceeds cut end {} ns, stopping", cluster.get_timestamp_ms(self.timecode_scale), end_ns);
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

    fn initialize(&mut self, time_scale: Option<u64>) -> Result<()> {
        if let Some(ts) = time_scale {
            self.output_timecode_scale = ts;
        }

        self.file
            .seek(SeekFrom::Start(self.initial_cluster_pos.position))?;
        Ok(())
    }

    fn initialize_with_cut(
        &mut self,
        time_scale: Option<u64>,
        seek_type: SeekType,
        start_ns: Option<u64>,
        end_ns: Option<u64>,
    ) -> Result<i64> {
        if let Some(ts) = time_scale {
            self.output_timecode_scale = ts;
        }

        if let Some(start) = start_ns {
            self.find_start_cluster(start)?;
            self.file
                .seek(SeekFrom::Start(self.initial_cluster_pos.position))?;
        }
        
        // Also find end cluster if end timestamp is provided
        if let Some(end) = end_ns {
            self.find_end_cluster(end)?;
        }

        self.cut_parameters = CutConfig {
            seek_type: seek_type.clone(),
            start_ns,
            end_ns,
        };

        let video_tracks = self.tracks.get_all_video_tracks();
        let duration_ns = self.get_duration();
        print!("Orig duration: {} ns", duration_ns.as_ref().ok().copied().unwrap_or(0));
        let _orig_end_ns: Option<u64> = match end_ns {
            Some(end) => Some(end),
            None => match duration_ns {
                Ok(dur) => Some(dur),
                Err(_) => None,
            },
        };

        let keyframe_pos_ns: i64 = match video_tracks.first() {
            Some(video_track_num) => {
                // we have a video track, so look for keyframes to determine start position
                match seek_type {
                    SeekType::SnapNearestKeyframe => {
                        self.initial_cluster_pos.get_closest_keyframe_timestamp_ns(
                            *video_track_num,
                            start_ns.unwrap_or(0) as i64,
                        )?
                    }
                    SeekType::DirtyCut => {
                        self.initial_cluster_pos.get_closest_keyframe_timestamp_ns(
                            *video_track_num,
                            start_ns.unwrap_or(0) as i64,
                        )?
                    }
                    SeekType::Freeze => self.initial_cluster_pos.get_keyframe_timestamp_ns(
                        *video_track_num,
                        start_ns.unwrap_or(0) as i64,
                        true,
                    )?, // use keyframe after start for freeze since we will use that keyframe as freeze frame
                    SeekType::Squeeze => self.initial_cluster_pos.get_keyframe_timestamp_ns(
                        *video_track_num,
                        start_ns.unwrap_or(0) as i64,
                        false,
                    )?, // use keyframe before start and squeeze all frames to zero
                }
            }
            None => start_ns.unwrap_or(0) as i64, // we only consider video keyframes for cutting, so if there are no video tracks just use the specified start timestamp as is
        };
        self.file
            .seek(SeekFrom::Start(self.initial_cluster_pos.position))?;

        Ok(keyframe_pos_ns - start_ns.unwrap_or(0) as i64)
    }
}
