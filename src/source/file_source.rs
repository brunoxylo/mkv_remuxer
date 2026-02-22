use super::{PreRollCalculator, SeekType, Source};
use crate::block_ext::{ClusterBlockExt, ClusterExt, TrackKind, TracksExt};
use crate::{Error, Result};
use log::debug;
use log4rs::append::file;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use super::CutConfig;

const SEEK_AFTER_END_TIME_NS :u64 = 50_000_000_000;

pub struct FileSource {
    file: File,
    path: PathBuf,
    timecode_scale: u64,
    output_timecode_scale: u64,
    tracks: Tracks,
    info: Info,
    chapters: Option<Chapters>,
    cut_parameters: CutConfig,
    /// Pre-roll calculator for codec-aware frame dependencies
    pre_roll_calculator: Option<PreRollCalculator>,
    /// Index of all clusters: (file_position, timestamp_ns)
    cluster_index: Vec<(u64, u64)>,
    /// Current cluster position for iteration
    current_cluster_idx: usize,
    first_video_track_num: Option<u64>,
    video_codec_id: Option<String>,
    finished: bool,
}

impl FileSource {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
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
        let mut first_cluster_pos: Option<u64> = None;

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
                first_cluster_pos = Some(pos);
                break;
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    file.seek(SeekFrom::Current(size as i64))?;
                }
            }
        }

        let _first_cluster_pos = first_cluster_pos
            .ok_or_else(|| Error::InvalidConfig("No clusters found".to_string()))?;

        let tracks_ref = tracks
            .as_ref()
            .ok_or_else(|| Error::InvalidConfig("Missing Tracks element".to_string()))?;

        let first_video_track_num = tracks_ref
            .get_all_video_tracks()
            .first()
            .cloned();

        // Get video codec ID for pre-roll calculator
        let video_codec_id: Option<String> = if let Some(video_track) = first_video_track_num {
            tracks_ref.track_entry.iter()
                .find(|t| t.track_number.0 == video_track)
                .map(|t| t.codec_id.0.clone())
        } else {
            None
        };

        Ok(Self {
            file,
            path: path_buf,
            timecode_scale,
            output_timecode_scale: timecode_scale,
            tracks: tracks
                .ok_or_else(|| Error::InvalidConfig("Missing Tracks element".to_string()))?,
            info: info.ok_or_else(|| Error::InvalidConfig("Missing Info element".to_string()))?,
            chapters,
            cut_parameters: CutConfig {
                seek_type: SeekType::Squeeze, // Only Squeeze mode supported now
                start_ns: None,
                end_ns: None,
            },
            pre_roll_calculator: None, // Will be initialized when needed
            cluster_index: Vec::new(), // Will be built on demand
            current_cluster_idx: 0,
            finished: false,
            first_video_track_num,
            video_codec_id,
        })
    }

    /// Build an index of all cluster positions and timestamps for binary search
    fn build_cluster_index(&mut self) -> Result<Vec<(u64, u64)>> {
        // Returns Vec of (file_position, timestamp_ns)
        let mut cluster_index = Vec::new();
        
        // Seek to beginning of clusters (after metadata)
        self.file.rewind()?;
        
        // Skip EBML and Segment headers, find first cluster
        let _ebml_header = Header::read_from(&mut self.file)?;
        let ebml_size = _ebml_header.size.value;
        if ebml_size > 0 && !_ebml_header.size.is_unknown {
            self.file.seek(SeekFrom::Current(ebml_size as i64))?;
        }
        
        let _segment_header = Header::read_from(&mut self.file)?;
        
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
                let timestamp_ns = cluster.get_timestamp_ns(self.timecode_scale);
                cluster_index.push((current_pos, timestamp_ns));
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    if self.file.seek(SeekFrom::Current(size as i64)).is_err() {
                        break;
                    }
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
        // Find index for this position
        self.current_cluster_idx = self.cluster_index.iter()
            .position(|(p, _)| *p == pos)
            .unwrap_or(0);
        Ok(())
    }
    
    fn find_end_cluster(&mut self, _target_timestamp_ns: u64) -> Result<()> {
        // For now, we'll process until natural end or cut point
        // TODO: Could optimize by finding exact end cluster
        Ok(())
    }

    fn process_cluster_for_cut(&mut self, mut cluster: Cluster) -> Result<Cluster> {
        if self.cut_parameters.start_ns.is_none() && self.cut_parameters.end_ns.is_none() {
            return Ok(cluster); // no cutting needed, just return original cluster
        }

        let orig_block_count = cluster.blocks.len();
        let orig_cluster_ticks = cluster.timestamp.0 as i64;
        let orig_cluster_ns = cluster.get_timestamp_ns(self.timecode_scale) as i64;

        // Squeeze mode: shift cluster timestamp based on start position
        let shift_reference = self.cut_parameters.start_ns.unwrap_or(0) as i64;
        let shifted_ns = orig_cluster_ns - shift_reference;
        cluster.timestamp.0 = (shifted_ns / self.output_timecode_scale as i64).max(0) as u64;
        let shifted_cluster_ticks = cluster.timestamp.0 as i64;

        // Process with Squeeze logic
        let result = self.process_squeeze_cluster(cluster, orig_cluster_ticks, shifted_cluster_ticks);

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

    fn process_squeeze_cluster(
        &self,
        mut cluster: Cluster,
        orig_cluster_ticks: i64,
        shifted_cluster_ticks: i64,
    ) -> Result<Cluster> {
        let mut filtered = Vec::with_capacity(cluster.blocks.len());
        let orig_cluster_ns = (orig_cluster_ticks * self.timecode_scale as i64) / 1_000_000;
        
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
        let own_file_name = self.path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        write!(f, "FileSource({})", own_file_name)
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

    fn get_duration(&mut self) -> Result<u64> {
        // Get original duration from Info element
        let orig_duration = match self.info.duration {
            Some(duration) => (duration.0 * self.timecode_scale as f64) as u64,
            None => return Err(Error::InvalidConfig("No duration in Info element".to_string())),
        };

        // If cutting, calculate duration based on cut parameters
        let start = self.cut_parameters.start_ns.unwrap_or(0);
        let end = self.cut_parameters.end_ns.unwrap_or(orig_duration);
        
        if end > start {
            Ok(end - start)
        } else {
            Err(Error::InvalidConfig("End timestamp is before start timestamp".to_string()))
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

                let cluster_ts_ns = cluster.get_timestamp_ns(self.timecode_scale);

                // Check if we should stop based on end time
                if let Some(end_ns) = self.cut_parameters.end_ns {
                    if cluster_ts_ns > end_ns + SEEK_AFTER_END_TIME_NS {
                        println!("Cluster at {} ns exceeds cut end {} ns, stopping", cluster_ts_ns, end_ns);
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

        // Build cluster index if not already built
        if self.cluster_index.is_empty() {
            self.cluster_index = self.build_cluster_index()?;
        }

        // Seek to first cluster
        if let Some((pos, _)) = self.cluster_index.first() {
            self.file.seek(SeekFrom::Start(*pos))?;
        }
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

        // Build cluster index if not already built
        if self.cluster_index.is_empty() {
            self.cluster_index = self.build_cluster_index()?;
        }

        // Initialize pre-roll calculator if video track exists
        if self.pre_roll_calculator.is_none() {
            if let Some(codec_id) = &self.video_codec_id {
                self.pre_roll_calculator = Some(PreRollCalculator::new(
                    self.file.try_clone()?,
                    self.timecode_scale,
                    codec_id,
                ));
            }
        }

        // Find starting cluster if start time specified
        if let Some(start) = start_ns {
            self.find_start_cluster(start)?;
            if let Some((pos, _)) = self.cluster_index.get(self.current_cluster_idx) {
                self.file.seek(SeekFrom::Start(*pos))?;
            }
        }
        
        // Note end cluster (used for optimization, not strictly necessary)
        if let Some(end) = end_ns {
            self.find_end_cluster(end)?;
        }

        self.cut_parameters = CutConfig {
            seek_type: seek_type.clone(),
            start_ns,
            end_ns,
        };

        // For Squeeze mode, we always cut at keyframe before start
        // Pre-roll frames will be handled by PreRollCalculator
        let keyframe_offset = 0i64; // We handle this via pre-roll now
        
        Ok(keyframe_offset)
    }
}
