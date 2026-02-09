use crate::{Result, Error};
use crate::block_ext::{ClusterBlockExt, TrackKind, TracksExt};
use super::{Source, SeekType};
use mkv_element::{ClusterBlock, prelude::*};
use mkv_element::io::blocking_impl::*;
use log::debug;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;


struct CutParameters {
    seek_type: SeekType,
    start_ns: Option<u64>,
    end_ns: Option<u64>,
    start_keyframe_time_ns: Option<u64>,
    keyframe_block: Option<ClusterBlock>,
}

pub struct FileSource {
    file: File,
    timecode_scale: u64,
    output_timecode_scale: u64,
    tracks: Tracks,
    info: Info,
    chapters: Option<Chapters>,
    cut_parameters: CutParameters,
    clusters_start_pos: u64,
    current_cluster: Option<Cluster>,
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
        let clusters_start_pos;
        
        loop {
            let pos = file.stream_position()?;
            let header = match Header::read_from(&mut file) {
                Ok(h) => h,
                Err(_) => {
                    clusters_start_pos = pos;
                    break;
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
                clusters_start_pos = pos;
                break;
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    file.seek(SeekFrom::Current(size as i64))?;
                }
            }
        }
        
        Ok(Self {
            file,
            timecode_scale,
            output_timecode_scale: timecode_scale,
            tracks: tracks.ok_or_else(|| Error::InvalidConfig("Missing Tracks element".to_string()))?,
            info: info.ok_or_else(|| Error::InvalidConfig("Missing Info element".to_string()))?,
            chapters,
            cut_parameters: CutParameters {
                seek_type: SeekType::SnapNearestKeyframe,
                start_ns: None,
                end_ns: None,
                start_keyframe_time_ns: None,
                keyframe_block: None,
            },
            clusters_start_pos,
            current_cluster: None,
            finished: false,
        })
    }
    
    fn find_keyframe_of_interest(&mut self, target_ns: u64, seek_type: &SeekType) -> Result<(u64,u64)> {
        self.file.seek(SeekFrom::Start(self.clusters_start_pos))?;
        let mut before_pos = self.clusters_start_pos;
        let mut before_time_ns = 0u64;
        let mut after_pos: Option<u64> = None;
        let mut after_time_ns: Option<u64> = None;
        
        loop {
            let pos = self.file.stream_position()?;
            let header = match Header::read_from(&mut self.file) {
                Ok(h) => h,
                Err(_) => break,
            };
            
            if header.id == Cluster::ID {
                let cluster = match Cluster::read_element(&header, &mut self.file) {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let cluster_time_ns = cluster.timestamp.0 * self.timecode_scale;
                
                // Check for video keyframe
                let has_keyframe = cluster.blocks.iter().any(|block| {
                    if let Ok(track_num) = block.track_number() {
                        if let Some(track) = self.tracks.get_track_kind(track_num) {
                            if track == TrackKind::Video {
                                self.cut_parameters.keyframe_block = Some(block.clone());
                                return block.is_keyframe().unwrap_or(false);
                            }
                        }
                    }
                    false
                });
                
                if has_keyframe {
                    if cluster_time_ns <= target_ns {
                        before_pos = pos;
                        before_time_ns = cluster_time_ns;
                    } else if cluster_time_ns > target_ns && after_pos.is_none() {
                        // First keyframe after target
                        after_pos = Some(pos);
                        after_time_ns = Some(cluster_time_ns);
                        
                        // For freeze mode, we want the first keyframe after, so stop here
                        if matches!(seek_type, SeekType::Freeze) {
                            break;
                        }
                        
                        // For snap mode, we have before and after, stop here
                        if matches!(seek_type, SeekType::SnapNearestKeyframe) {
                            break;
                        }
                    }
                }
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    self.file.seek(SeekFrom::Current(size as i64))?;
                }
            }
        }
        
        // Choose keyframe based on seek type
        match seek_type {
            SeekType::SnapNearestKeyframe => {
                // Return the closest keyframe
                if let (Some(after_pos), Some(after_time_ns)) = (after_pos, after_time_ns) {
                    let before_distance = target_ns.saturating_sub(before_time_ns);
                    let after_distance = after_time_ns.saturating_sub(target_ns);
                    
                    if after_distance < before_distance {
                        Ok((after_pos, after_time_ns))
                    } else {
                        Ok((before_pos, before_time_ns))
                    }
                } else {
                    // No keyframe after, use before
                    Ok((before_pos, before_time_ns))
                }
            }
            SeekType::Freeze => {
                // Use first keyframe after target
                if let (Some(after_pos), Some(after_time_ns)) = (after_pos, after_time_ns) {
                    Ok((before_pos, after_time_ns))
                } else {
                    // No keyframe after, use before as fallback
                    Ok((before_pos, before_time_ns))
                }
            }
            SeekType::Squeeze | SeekType::DirtyCut => {
                // Use keyframe before target
                Ok((before_pos, before_time_ns))
            }
        }
    }
    
    fn process_cluster_for_cut(&mut self, mut cluster: Cluster) -> Result<Cluster> {
        if self.cut_parameters.start_keyframe_time_ns.is_none() {
            return Ok(cluster); // no cutting needed, just return original cluster
        }
        
        let orig_block_count = cluster.blocks.len();
        
        let orig_cluster_ticks = cluster.timestamp.0 as i64;
        let cluster_ns = orig_cluster_ticks * self.timecode_scale as i64;
        
        // Shift cluster timestamp
        // For squeeze mode, shift by start time so blocks align correctly
        // For freeze and other modes, shift by keyframe time
        let shift_reference = self.cut_parameters.start_ns.unwrap_or(0);
        let shifted_ns = cluster_ns - shift_reference as i64;
        cluster.timestamp.0 = (shifted_ns / self.output_timecode_scale as i64).max(0) as u64;
        let shifted_cluster_ticks = cluster.timestamp.0 as i64;
        
        let mut filtered = Vec::new();
        
        let result = match self.cut_parameters.seek_type {
            SeekType::SnapNearestKeyframe => {
                // Simple: just shift timestamps, no filtering needed
                Ok(cluster)
            }
            SeekType::DirtyCut => {
                // Drop frames outside range
                for block in cluster.blocks {
                    let abs_ns = block.timestamp_ns(orig_cluster_ticks, self.timecode_scale).ok();
                    if let Some(abs_ns) = abs_ns {
                        let abs_ns = abs_ns as u64;
                        if let Some(start) = self.cut_parameters.start_ns {
                            if abs_ns < start { continue; }
                        }
                        if let Some(end) = self.cut_parameters.end_ns {
                            if abs_ns > end { continue; }
                        }
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
                debug!("Cluster filtering removed all {} blocks (cluster_ns={}, start_ns={:?}, end_ns={:?})", 
                    orig_block_count, cluster_ns, self.cut_parameters.start_ns, self.cut_parameters.end_ns);
            }
        }
        
        result
    }
    
    fn process_freeze_cluster(
        &self,
        mut cluster: Cluster,
        orig_cluster_ticks: i64,
        shifted_cluster_ticks: i64,
    ) -> Result<Cluster> {
        let start_ns = self.cut_parameters.start_ns.unwrap_or(0);
        let mut is_freeze_frame_set = false;
        let mut filtered = Vec::new();        
        for mut block in cluster.blocks {
            let track_num = block.track_number()?;
            let kind = self.tracks.get_track_kind(track_num)
                .ok_or_else(|| Error::TrackNotFound(track_num))?;
            
            let abs_ns = block.timestamp_ns(orig_cluster_ticks, self.timecode_scale)
                .unwrap_or(0) as u64;
            
            match kind {
                TrackKind::Video => {
                    // Drop video after end
                    if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end { continue; }
                    }
                    
                    if abs_ns < start_ns {
                        continue; // Drop non-keyframes before start
                    }  
                    debug!("abs_ns {} start_ns {} keyframe_time {}", abs_ns, start_ns, self.cut_parameters.start_keyframe_time_ns.unwrap_or(0));
                        
                    if abs_ns >= (start_ns) && abs_ns<= (self.cut_parameters.start_keyframe_time_ns.unwrap_or(0)) {
                        debug!("inside freeze zone");

                        if !is_freeze_frame_set {
                            if let Some(kf) = &self.cut_parameters.keyframe_block    {
                                block = kf.clone();
                            debug!("FOUND KEYFRAME");
                            block.set_timestamp_ns(0, cluster.timestamp.0 as i64 , self.output_timecode_scale)?; //set first keyframe to time 0
                            is_freeze_frame_set = true;
                        }
                        } else {
                            continue; // Drop non-keyframes before first keyframe
                        }
                        
                    } else {
                        // Shift video
                        let offset = abs_ns as i64 - start_ns as i64;
                        block.set_timestamp_ns(offset, cluster.timestamp.0 as i64, self.output_timecode_scale)?;
                    }
                }
                _ => { // all other tracks just shift timestamps if within range
                        // Drop after end
                        if let Some(end) = self.cut_parameters.end_ns {
                            if abs_ns > end { continue; }
                        }
                        // Drop before start
                        if abs_ns < start_ns { continue; }
                        // Shift audio
                        let offset = abs_ns as i64 - start_ns as i64;
                        block.set_timestamp_ns(offset, cluster.timestamp.0 as i64, self.output_timecode_scale)?;
                    } // Other tracks: just shift timestamps
            }
            
            filtered.push(block);
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
        let start_ns = self.cut_parameters.start_ns.unwrap_or(0);
        let squeeze_window_ns = 10_000_000i64; // 10ms window
        let mut filtered = Vec::new();
        
        for mut block in cluster.blocks {
            let track_num = block.track_number()?;
            let kind = self.tracks.get_track_kind(track_num)
                .ok_or_else(|| Error::TrackNotFound(track_num))?;
            
            let abs_ns = block.timestamp_ns(orig_cluster_ticks, self.timecode_scale)
                .unwrap_or(0);
            
            match kind {
                TrackKind::Audio => {
                    // Drop audio before start (pre-roll is video-only)
                    if abs_ns < start_ns as i64 { continue; }
                    // Drop audio after end
                    if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end as i64 { continue; }
                    }
                    // Shift audio to start after squeeze window
                    let offset = abs_ns - start_ns as i64;
                    let new_ns = squeeze_window_ns + offset;
                    block.set_timestamp_ns(new_ns, shifted_cluster_ticks, self.output_timecode_scale)?;
                }
                TrackKind::Video => {
                    if abs_ns < start_ns as i64 {
                        // Pre-roll: squeeze to time 0 and mark invisible
                        block.set_timestamp_ns(0, shifted_cluster_ticks, self.output_timecode_scale)?;
                        block.set_invisible(true)?;
                    } else if let Some(end) = self.cut_parameters.end_ns {
                        if abs_ns > end as i64 {
                            continue; // Drop post-roll for now (could squeeze at end)
                        } else {
                            // Main content: shift by squeeze window
                            let offset = abs_ns - start_ns as i64;
                            let new_ns = squeeze_window_ns + offset;
                            block.set_timestamp_ns(new_ns, shifted_cluster_ticks, self.output_timecode_scale)?;
                        }
                    } else {
                        // No end: just shift by squeeze window
                        let offset = abs_ns - start_ns as i64;
                        let new_ns = squeeze_window_ns + offset;
                        block.set_timestamp_ns(new_ns, shifted_cluster_ticks, self.output_timecode_scale)?;
                    }
                }
                _ => {} // Other tracks: just shift timestamps
            }
            
            filtered.push(block);
        }
        
        cluster.blocks = filtered;
        Ok(cluster)
    }
}

impl fmt::Display for FileSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FileSource",
        )
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
                    let cluster_time_ns = cluster.timestamp.0 * self.timecode_scale;
                    // Allow some buffer for post-roll frames
                    if cluster_time_ns > end_ns + 50_000_000_000 {
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
        
        self.file.seek(SeekFrom::Start(self.clusters_start_pos))?;
        Ok(())
    }
    
    fn initialize_with_cut(
        &mut self,
        time_scale: Option<u64>,
        seek_type: SeekType,
        start_ns: Option<u64>,
        end_ns: Option<u64>,
    ) -> Result<(u64, u64)> {
        if let Some(ts) = time_scale {
            self.output_timecode_scale = ts;
        }
        
        
        let start = start_ns.unwrap_or(0);
        let (keyframe_pos, keyframe_time_ns) = self.find_keyframe_of_interest(start, &seek_type)?;
        
        // Calculate offsets from requested positions to actual keyframe positions
        let start_offset = start.saturating_sub(keyframe_time_ns);
        let end_offset = if let Some(_end) = end_ns {
            // For end, we'd need to find the keyframe after end, but for now just return 0
            0
        } else {
            0
        };
        
        self.cut_parameters = CutParameters {
            seek_type,
            start_ns,
            end_ns,
            start_keyframe_time_ns: Some(keyframe_time_ns),
            keyframe_block: self.cut_parameters.keyframe_block.clone(),
        };
        
        // Seek to keyframe position
        self.file.seek(SeekFrom::Start(keyframe_pos))?;
        
        // Update info duration if we have both start and end
        if let (Some(start), Some(end)) = (start_ns, end_ns) {
            let duration_ns = end.saturating_sub(start);
            self.info.duration = Some(Duration(duration_ns as f64 / self.output_timecode_scale as f64));
        }
        
        Ok((start_offset, end_offset))
    }
}

