// Unified video cutting module supporting multiple approaches:
// - Freeze: Pre-roll frames frozen at timestamp 0
// - Squeeze: Pre/post-roll frames squeezed into timestamp 0
// - Snap: Cut at nearest keyframe boundaries

use anyhow::{Context, Result};
use mkv_element::ClusterBlock;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Seek, SeekFrom, Write};

fn vint_length(byte: u8) -> usize {
    if byte & 0x80 != 0 {
        1
    } else if byte & 0x40 != 0 {
        2
    } else if byte & 0x20 != 0 {
        3
    } else if byte & 0x10 != 0 {
        4
    } else if byte & 0x08 != 0 {
        5
    } else if byte & 0x04 != 0 {
        6
    } else if byte & 0x02 != 0 {
        7
    } else if byte & 0x01 != 0 {
        8
    } else {
        0
    }
}

/// Extension trait for reading SimpleBlock data
trait SimpleBlockExt {
    fn get_timestamp(&self, cluster_timestamp: i64, timecode_scale: u64) -> Option<i64>;
    fn is_invisible(&self) -> Option<bool>;
}

/// Extension trait for modifying SimpleBlock data
trait SimpleBlockExtMut {
    fn set_timestamp(&mut self, time_in_ns: i64, cluster_timestamp: i64, timecode_scale: u64) -> Result<()>;
    fn set_invisible(&mut self, invisible: bool) -> Result<()>;
}

impl SimpleBlockExt for [u8] {
    fn get_timestamp(&self, cluster_timestamp: i64, timecode_scale: u64) -> Option<i64> {
        if self.len() < 4 {
            return None;
        }
        let track_len = vint_length(self[0]);
        let tc_bytes = [self[track_len], self[track_len + 1]];
        let rel_ticks = i16::from_be_bytes(tc_bytes);
        let abs_ticks = cluster_timestamp + rel_ticks as i64;
        let abs_ns = abs_ticks * timecode_scale as i64;
        Some(abs_ns)
    }
    
    fn is_invisible(&self) -> Option<bool> {
        if self.len() < 4 {
            return None;
        }
        let track_len = vint_length(self[0]);
        Some((self[track_len + 2] & 0x08) != 0)
    }
}

impl SimpleBlockExtMut for [u8] {
    fn set_timestamp(&mut self, time_in_ns: i64, cluster_timestamp: i64, timecode_scale: u64) -> Result<()> {
        if self.len() < 4 {
            anyhow::bail!("SimpleBlock data too short");
        }
        let track_len = vint_length(self[0]);
        let new_ticks = time_in_ns / timecode_scale as i64;
        let new_rel_ticks = new_ticks - cluster_timestamp;
        let clamped = new_rel_ticks.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        let new_tc_bytes = clamped.to_be_bytes();
        self[track_len] = new_tc_bytes[0];
        self[track_len + 1] = new_tc_bytes[1];
        Ok(())
    }
    
    fn set_invisible(&mut self, invisible: bool) -> Result<()> {
        if self.len() < 4 {
            anyhow::bail!("SimpleBlock data too short");
        }
        let track_len = vint_length(self[0]);
        if invisible {
            self[track_len + 2] |= 0x08;
        } else {
            self[track_len + 2] &= !0x08;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
enum TrackKind {
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CutMode {
    /// Freeze frames before start at timestamp 0
    Freeze,
    /// Squeeze pre/post-roll frames into timestamp 0
    Squeeze,
    /// Snap to nearest keyframe boundaries
    Snap,
}

pub struct CutOptions {
    pub mode: CutMode,
    pub start_ns: u64,
    pub end_ns: Option<u64>,
    /// If Some, only export these track numbers. If None, export all tracks.
    pub track_filter: Option<Vec<u64>>,
}

pub struct CutResult {
    /// Offset at start in seconds (keyframe offset for snap, freeze duration for freeze, squeeze duration for squeeze)
    pub start_offset_s: f64,
    /// Offset at end in seconds
    pub end_offset_s: f64,
    /// Output data stream
    pub data: Cursor<Vec<u8>>,
}

pub fn cut_video(input_path: &str, options: CutOptions) -> Result<CutResult> {
    match options.mode {
        CutMode::Freeze => cut_freeze(input_path, options),
        CutMode::Squeeze => cut_squeeze(input_path, options),
        CutMode::Snap => cut_snap(input_path, options),
    }
}

fn cut_snap(input_path: &str, options: CutOptions) -> Result<CutResult> {
    let mut input_file = File::open(input_path).context("Failed to open input file")?;
    let mut output = Vec::new();
    let mut writer = std::io::Cursor::new(&mut output);

    // 1. EBML Header
    let ebml_header = Header::read_from(&mut input_file)?;
    let ebml = Ebml::read_element(&ebml_header, &mut input_file)?;
    ebml.write_to(&mut writer)?;

    // 2. Segment Header
    let segment_header = Header::read_from(&mut input_file)?;
    if segment_header.id.value != Segment::ID.value {
        anyhow::bail!("Expected Segment, found ID: {:x}", segment_header.id.value);
    }
    let mut out_segment_header = segment_header.clone();
    out_segment_header.size = VInt64::new_unknown();
    out_segment_header.write_to(&mut writer)?;

    // 3. Scan headers
    let mut timecode_scale = 1_000_000;
    let mut stored_info: Option<Info> = None;
    let mut stored_tracks: Option<Tracks> = None;
    let clusters_start_pos: u64;

    loop {
        let pos = input_file.stream_position()?;
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => {
                clusters_start_pos = pos;
                break;
            }
        };

        if header.id == Info::ID {
            let info = Info::read_element(&header, &mut input_file)?;
            timecode_scale = info.timestamp_scale.0;
            stored_info = Some(info);
        } else if header.id == Tracks::ID {
            stored_tracks = Some(Tracks::read_element(&header, &mut input_file)?);
        } else if header.id == Cluster::ID {
            clusters_start_pos = pos;
            break;
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    // Build track map
    let mut track_map = HashMap::new();
    if let Some(ref t) = stored_tracks {
        for track in &t.track_entry {
            let kind = match track.track_type.0 {
                1 => TrackKind::Video,
                2 => TrackKind::Audio,
                _ => TrackKind::Other,
            };
            track_map.insert(track.track_number.0, kind);
        }
    }

    // Find nearest keyframe to start
    input_file.seek(SeekFrom::Start(clusters_start_pos))?;
    let mut start_keyframe_pos = clusters_start_pos;
    let mut start_keyframe_time_ns = 0u64;
    let mut start_cluster_ticks = 0u64;
    
    let mut keyframe_before_pos = clusters_start_pos;
    let mut keyframe_before_time_ns = 0u64;
    let mut keyframe_before_ticks = 0u64;

    loop {
        let pos = input_file.stream_position()?;
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break,
        };
        if header.id == Cluster::ID {
            let cluster = match Cluster::read_element(&header, &mut input_file) {
                Ok(c) => c,
                Err(_) => break,
            };
            let cluster_time_ns = cluster.timestamp.0 * timecode_scale;
            let mut has_video_keyframe = false;
            
            for block in &cluster.blocks {
                if let ClusterBlock::Simple(sb) = block {
                    let data = &sb.0;
                    if data.len() < 4 {
                        continue;
                    }
                    let track_len = vint_length(data[0]);
                    let mut cursor = std::io::Cursor::new(&data[..]);
                    let track_num = VInt64::read_from(&mut cursor)
                        .unwrap_or(VInt64::new(0))
                        .value;
                    if let Some(TrackKind::Video) = track_map.get(&track_num) {
                        if (data[track_len + 2] & 0x80) != 0 {
                            has_video_keyframe = true;
                            break;
                        }
                    }
                }
            }

            if has_video_keyframe {
                if cluster_time_ns <= options.start_ns {
                    keyframe_before_pos = pos;
                    keyframe_before_time_ns = cluster_time_ns;
                    keyframe_before_ticks = cluster.timestamp.0;
                } else {
                    let dist_before = options.start_ns.saturating_sub(keyframe_before_time_ns);
                    let dist_after = cluster_time_ns.saturating_sub(options.start_ns);
                    
                    if dist_after < dist_before {
                        start_keyframe_pos = pos;
                        start_keyframe_time_ns = cluster_time_ns;
                        start_cluster_ticks = cluster.timestamp.0;
                    } else {
                        start_keyframe_pos = keyframe_before_pos;
                        start_keyframe_time_ns = keyframe_before_time_ns;
                        start_cluster_ticks = keyframe_before_ticks;
                    }
                    break;
                }
            }
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }
    
    if start_keyframe_pos == clusters_start_pos && keyframe_before_pos != clusters_start_pos {
        start_keyframe_pos = keyframe_before_pos;
        start_keyframe_time_ns = keyframe_before_time_ns;
        start_cluster_ticks = keyframe_before_ticks;
    }

    // Find nearest keyframe to end
    let mut end_keyframe_time_ns = None;
    if let Some(end) = options.end_ns {
        input_file.seek(SeekFrom::Start(start_keyframe_pos))?;
        
        let mut keyframe_before_end_time_ns = None;
        
        loop {
            let header = match Header::read_from(&mut input_file) {
                Ok(h) => h,
                Err(_) => break,
            };
            if header.id == Cluster::ID {
                let cluster = match Cluster::read_element(&header, &mut input_file) {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let cluster_time_ns = cluster.timestamp.0 * timecode_scale;
                let mut has_video_keyframe = false;
                
                for block in &cluster.blocks {
                    if let ClusterBlock::Simple(sb) = block {
                        let data = &sb.0;
                        if data.len() < 4 {
                            continue;
                        }
                        let track_len = vint_length(data[0]);
                        let mut cursor = std::io::Cursor::new(&data[..]);
                        let track_num = VInt64::read_from(&mut cursor)
                            .unwrap_or(VInt64::new(0))
                            .value;
                        if let Some(TrackKind::Video) = track_map.get(&track_num) {
                            if (data[track_len + 2] & 0x80) != 0 {
                                has_video_keyframe = true;
                                break;
                            }
                        }
                    }
                }

                if has_video_keyframe {
                    if cluster_time_ns <= end {
                        keyframe_before_end_time_ns = Some(cluster_time_ns);
                    } else {
                        if let Some(before_ns) = keyframe_before_end_time_ns {
                            let dist_before = end.saturating_sub(before_ns);
                            let dist_after = cluster_time_ns.saturating_sub(end);
                            
                            if dist_after < dist_before {
                                end_keyframe_time_ns = Some(cluster_time_ns);
                            } else {
                                end_keyframe_time_ns = Some(before_ns);
                            }
                        } else {
                            end_keyframe_time_ns = Some(cluster_time_ns);
                        }
                        break;
                    }
                }
            } else {
                let size = header.size.value;
                if size > 0 && !header.size.is_unknown {
                    input_file.seek(SeekFrom::Current(size as i64))?;
                }
            }
        }
        
        if end_keyframe_time_ns.is_none() {
            end_keyframe_time_ns = keyframe_before_end_time_ns;
        }
    }

    let start_offset_s = (start_keyframe_time_ns as i64 - options.start_ns as i64).abs() as f64 / 1_000_000_000.0;
    let end_offset_s = end_keyframe_time_ns.map(|end_kf| 
        (end_kf as i64 - options.end_ns.unwrap_or(0) as i64).abs() as f64 / 1_000_000_000.0
    ).unwrap_or(0.0);

    // Write headers
    if let Some(mut info) = stored_info {
        if let Some(end_kf_ns) = end_keyframe_time_ns {
            let duration_ns = end_kf_ns.saturating_sub(start_keyframe_time_ns);
            info.duration = Some(Duration(duration_ns as f64 / timecode_scale as f64));
        }
        info.write_to(&mut writer)?;
    }
    
    if let Some(mut tracks) = stored_tracks {
        // Filter tracks if specified
        if let Some(ref filter) = options.track_filter {
            tracks.track_entry.retain(|track| filter.contains(&track.track_number.0));
        }
        tracks.write_to(&mut writer)?;
    }

    // Copy clusters
    input_file.seek(SeekFrom::Start(start_keyframe_pos))?;

    loop {
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break,
        };
        if header.id == Cluster::ID {
            let mut cluster = Cluster::read_element(&header, &mut input_file)?;
            let cluster_time_ns = cluster.timestamp.0 * timecode_scale;
            
            if let Some(end_kf_ns) = end_keyframe_time_ns {
                if cluster_time_ns >= end_kf_ns {
                    break;
                }
            }
            
            cluster.timestamp.0 = cluster.timestamp.0.saturating_sub(start_cluster_ticks);
            
            // Filter blocks by track
            if let Some(ref filter) = options.track_filter {
                cluster.blocks.retain(|block| {
                    if let ClusterBlock::Simple(sb) = block {
                        if sb.0.len() >= 1 {
                            let mut cursor = std::io::Cursor::new(&sb.0[..]);
                            if let Ok(track_num) = VInt64::read_from(&mut cursor) {
                                return filter.contains(&track_num.value);
                            }
                        }
                    }
                    true
                });
            }
            
            cluster.write_to(&mut writer)?;
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            } else {
                break;
            }
        }
    }

    writer.flush()?;
    drop(writer);
    
    Ok(CutResult {
        start_offset_s,
        end_offset_s,
        data: Cursor::new(output),
    })
}

fn cut_freeze(input_path: &str, options: CutOptions) -> Result<CutResult> {
    let mut input_file = File::open(input_path)?;
    let mut output = Vec::new();
    let mut writer = std::io::Cursor::new(&mut output);

    // Headers
    let ebml_header = Header::read_from(&mut input_file)?;
    let ebml = Ebml::read_element(&ebml_header, &mut input_file)?;
    ebml.write_to(&mut writer)?;

    let segment_header = Header::read_from(&mut input_file)?;
    if segment_header.id.value != Segment::ID.value {
        anyhow::bail!("Expected Segment");
    }
    let mut out_segment_header = segment_header.clone();
    out_segment_header.size = VInt64::new_unknown();
    out_segment_header.write_to(&mut writer)?;

    let mut timecode_scale = 1_000_000;
    let mut stored_info: Option<Info> = None;
    let mut stored_tracks: Option<Tracks> = None;
    let clusters_start_pos: u64;

    loop {
        let pos = input_file.stream_position()?;
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => {
                clusters_start_pos = pos;
                break;
            }
        };

        if header.id == Info::ID {
            let info = Info::read_element(&header, &mut input_file)?;
            timecode_scale = info.timestamp_scale.0;
            stored_info = Some(info);
        } else if header.id == Tracks::ID {
            stored_tracks = Some(Tracks::read_element(&header, &mut input_file)?);
        } else if header.id == Cluster::ID {
            clusters_start_pos = pos;
            break;
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    let mut track_map = HashMap::new();
    if let Some(ref t) = stored_tracks {
        for track in &t.track_entry {
            let kind = match track.track_type.0 {
                1 => TrackKind::Video,
                2 => TrackKind::Audio,
                _ => TrackKind::Other,
            };
            track_map.insert(track.track_number.0, kind);
        }
    }

    // Find keyframe before start
    input_file.seek(SeekFrom::Start(clusters_start_pos))?;
    let mut best_start_pos = clusters_start_pos;
    let mut best_start_time_ns = 0u64;

    loop {
        let pos = input_file.stream_position()?;
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break,
        };
        if header.id == Cluster::ID {
            let cluster = match Cluster::read_element(&header, &mut input_file) {
                Ok(c) => c,
                Err(_) => break,
            };
            let cluster_time_ns = cluster.timestamp.0 * timecode_scale;
            let mut has_video_keyframe = false;
            
            for block in &cluster.blocks {
                if let ClusterBlock::Simple(sb) = block {
                    let data = &sb.0;
                    if data.len() < 4 {
                        continue;
                    }
                    let track_len = vint_length(data[0]);
                    let mut cursor = std::io::Cursor::new(&data[..]);
                    let track_num = VInt64::read_from(&mut cursor)
                        .unwrap_or(VInt64::new(0))
                        .value;
                    if let Some(TrackKind::Video) = track_map.get(&track_num) {
                        if (data[track_len + 2] & 0x80) != 0 {
                            has_video_keyframe = true;
                            break;
                        }
                    }
                }
            }

            if has_video_keyframe {
                if cluster_time_ns <= options.start_ns {
                    best_start_pos = pos;
                    best_start_time_ns = cluster_time_ns;
                } else {
                    break;
                }
            } else if cluster_time_ns > options.start_ns + 5_000_000_000 {
                break;
            }
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    let freeze_duration_s = (options.start_ns.saturating_sub(best_start_time_ns)) as f64 / 1_000_000_000.0;

    // Write headers
    if let Some(mut info) = stored_info {
        if let Some(end) = options.end_ns {
            let dur_ns = end.saturating_sub(options.start_ns);
            info.duration = Some(Duration(dur_ns as f64 / timecode_scale as f64));
        }
        info.write_to(&mut writer)?;
    }
    
    if let Some(mut tracks) = stored_tracks {
        if let Some(ref filter) = options.track_filter {
            tracks.track_entry.retain(|track| filter.contains(&track.track_number.0));
        }
        tracks.write_to(&mut writer)?;
    }

    // Remux clusters
    input_file.seek(SeekFrom::Start(best_start_pos))?;

    while let Ok(header) = Header::read_from(&mut input_file) {
        if header.id == Cluster::ID {
            let mut cluster = Cluster::read_element(&header, &mut input_file)?;
            let orig_cluster_ticks = cluster.timestamp.0;
            
            let cluster_ns = orig_cluster_ticks * timecode_scale;
            let shifted_ns = cluster_ns.saturating_sub(options.start_ns);
            cluster.timestamp.0 = shifted_ns / timecode_scale;
            let shifted_cluster_ticks = cluster.timestamp.0 as i64;

            let mut filtered = Vec::new();
            
            for block_enum in cluster.blocks {
                if let ClusterBlock::Simple(mut sb) = block_enum {
                    let data = &mut sb.0;
                    if data.len() < 4 {
                        continue;
                    }
                    let track_len = vint_length(data[0]);
                    let mut cursor = std::io::Cursor::new(&data[..]);
                    let track_num = VInt64::read_from(&mut cursor)
                        .unwrap_or(VInt64::new(0))
                        .value;
                    
                    // Filter by track
                    if let Some(ref filter) = options.track_filter {
                        if !filter.contains(&track_num) {
                            continue;
                        }
                    }
                    
                    let kind = track_map.get(&track_num).unwrap_or(&TrackKind::Other);

                    let tc_bytes = [data[track_len], data[track_len + 1]];
                    let rel_ticks = i16::from_be_bytes(tc_bytes);
                    let abs_ns =
                        (orig_cluster_ticks as i64 + rel_ticks as i64) as u64 * timecode_scale;

                    if *kind == TrackKind::Audio {
                        if abs_ns < options.start_ns {
                            continue;
                        }
                        
                        if let Some(end) = options.end_ns {
                            if abs_ns > end {
                                continue;
                            }
                        }
                        
                        let audio_offset_ns = abs_ns - options.start_ns;
                        let new_audio_ticks = audio_offset_ns / timecode_scale;
                        let new_rel_ticks = new_audio_ticks as i64 - shifted_cluster_ticks;
                        let clamped = new_rel_ticks.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
                        let new_tc_bytes = clamped.to_be_bytes();
                        data[track_len] = new_tc_bytes[0];
                        data[track_len + 1] = new_tc_bytes[1];
                    } 
                    else if *kind == TrackKind::Video {
                        if let Some(end) = options.end_ns {
                            if abs_ns > end {
                                continue;
                            }
                        }
                        
                        if abs_ns < options.start_ns {
                            // Freeze at 0
                            let new_rel_ticks = -shifted_cluster_ticks;
                            let clamped = new_rel_ticks.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
                            let new_tc_bytes = clamped.to_be_bytes();
                            data[track_len] = new_tc_bytes[0];
                            data[track_len + 1] = new_tc_bytes[1];
                        } else {
                            let video_offset_ns = abs_ns - options.start_ns;
                            let new_video_ticks = video_offset_ns / timecode_scale;
                            let new_rel_ticks = new_video_ticks as i64 - shifted_cluster_ticks;
                            let clamped = new_rel_ticks.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
                            let new_tc_bytes = clamped.to_be_bytes();
                            data[track_len] = new_tc_bytes[0];
                            data[track_len + 1] = new_tc_bytes[1];
                        }
                    }
                    
                    filtered.push(ClusterBlock::Simple(sb));
                } else {
                    filtered.push(block_enum);
                }
            }
            
            cluster.blocks = filtered;
            cluster.write_to(&mut writer)?;
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    writer.flush()?;
    drop(writer);
    
    Ok(CutResult {
        start_offset_s: freeze_duration_s,
        end_offset_s: 0.0,
        data: Cursor::new(output),
    })
}

fn cut_squeeze(input_path: &str, options: CutOptions) -> Result<CutResult> {
    let mut input_file = File::open(input_path)?;
    let mut output = Vec::new();
    let mut writer = std::io::Cursor::new(&mut output);

    // Headers
    let ebml_header = Header::read_from(&mut input_file)?;
    let ebml = Ebml::read_element(&ebml_header, &mut input_file)?;
    ebml.write_to(&mut writer)?;

    let segment_header = Header::read_from(&mut input_file)?;
    if segment_header.id.value != Segment::ID.value {
        anyhow::bail!("Expected Segment");
    }
    let mut out_segment_header = segment_header.clone();
    out_segment_header.size = VInt64::new_unknown();
    out_segment_header.write_to(&mut writer)?;

    let mut timecode_scale = 1_000_000;
    let mut stored_info: Option<Info> = None;
    let mut stored_tracks: Option<Tracks> = None;
    let clusters_start_pos: u64;

    loop {
        let pos = input_file.stream_position()?;
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => {
                clusters_start_pos = pos;
                break;
            }
        };

        if header.id == Info::ID {
            let info = Info::read_element(&header, &mut input_file)?;
            timecode_scale = info.timestamp_scale.0;
            stored_info = Some(info);
        } else if header.id == Tracks::ID {
            stored_tracks = Some(Tracks::read_element(&header, &mut input_file)?);
        } else if header.id == Cluster::ID {
            clusters_start_pos = pos;
            break;
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    let mut track_map = HashMap::new();
    if let Some(ref t) = stored_tracks {
        for track in &t.track_entry {
            let kind = match track.track_type.0 {
                1 => TrackKind::Video,
                2 => TrackKind::Audio,
                _ => TrackKind::Other,
            };
            track_map.insert(track.track_number.0, kind);
        }
    }

    // Find keyframe before start
    input_file.seek(SeekFrom::Start(clusters_start_pos))?;
    let mut best_start_pos = clusters_start_pos;
    let mut best_start_cluster_ticks = 0u64;

    loop {
        let pos = input_file.stream_position()?;
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break,
        };
        if header.id == Cluster::ID {
            let cluster = match Cluster::read_element(&header, &mut input_file) {
                Ok(c) => c,
                Err(_) => break,
            };
            let cluster_time_ns = cluster.timestamp.0 * timecode_scale;
            let mut has_video_keyframe = false;
            
            for block in &cluster.blocks {
                if let ClusterBlock::Simple(sb) = block {
                    let data = &sb.0;
                    if data.len() < 4 {
                        continue;
                    }
                    let track_len = vint_length(data[0]);
                    let mut cursor = std::io::Cursor::new(&data[..]);
                    let track_num = VInt64::read_from(&mut cursor)
                        .unwrap_or(VInt64::new(0))
                        .value;
                    if let Some(TrackKind::Video) = track_map.get(&track_num) {
                        if (data[track_len + 2] & 0x80) != 0 {
                            has_video_keyframe = true;
                            break;
                        }
                    }
                }
            }

            if has_video_keyframe {
                if cluster_time_ns <= options.start_ns {
                    best_start_pos = pos;
                    best_start_cluster_ticks = cluster.timestamp.0;
                } else {
                    break;
                }
            } else if cluster_time_ns > options.start_ns + 50_000_000_000 {
                break;
            }
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    let squeeze_start_ns = 10_000_000u64;
    let squeeze_end_ns = 10_000_000u64;

    // Write headers
    if let Some(mut info) = stored_info {
        if let Some(end) = options.end_ns {
            let content_dur_ns = end.saturating_sub(options.start_ns);
            let total_dur_ns = squeeze_start_ns + content_dur_ns;
            info.duration = Some(Duration(total_dur_ns as f64 / timecode_scale as f64));
        }
        info.write_to(&mut writer)?;
    }
    
    if let Some(mut tracks) = stored_tracks {
        if let Some(ref filter) = options.track_filter {
            tracks.track_entry.retain(|track| filter.contains(&track.track_number.0));
        }
        tracks.write_to(&mut writer)?;
    }

    // First pass: count pre/post-roll frames
    input_file.seek(SeekFrom::Start(best_start_pos))?;
    let mut video_frames: Vec<i64> = Vec::new();
    
    while let Ok(header) = Header::read_from(&mut input_file) {
        if header.id == Cluster::ID {
            let cluster = Cluster::read_element(&header, &mut input_file)?;
            let orig_cluster_ticks = cluster.timestamp.0;
            
            for block_enum in &cluster.blocks {
                if let ClusterBlock::Simple(sb) = block_enum {
                    let data = &sb.0;
                    if data.len() < 4 {
                        continue;
                    }
                    let mut cursor = std::io::Cursor::new(&data[..]);
                    let track_num = VInt64::read_from(&mut cursor)
                        .unwrap_or(VInt64::new(0))
                        .value;
                    let kind = track_map.get(&track_num).unwrap_or(&TrackKind::Other);
                    
                    if *kind == TrackKind::Video {
                        let abs_ns = data.get_timestamp(orig_cluster_ticks as i64, timecode_scale)
                            .unwrap_or(0);
                        
                        if let Some(end) = options.end_ns {
                            if abs_ns > (end + 50_000_000_000) as i64 {
                                break;
                            }
                        }
                        
                        video_frames.push(abs_ns);
                    }
                }
            }
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            } else {
                break;
            }
        }
    }
    
    let pre_roll_count = video_frames.iter().filter(|&&ts| ts < options.start_ns as i64).count();
    let post_roll_count = options.end_ns.map(|end| video_frames.iter().filter(|&&ts| ts > end as i64).count()).unwrap_or(0);

    // Calculate actual squeeze durations
    let pre_roll_frames: Vec<_> = video_frames.iter().filter(|&&ts| ts < options.start_ns as i64).copied().collect();
    let post_roll_frames: Vec<_> = options.end_ns.map(|end| 
        video_frames.iter().filter(|&&ts| ts > end as i64).copied().collect()
    ).unwrap_or_default();
    
    let squeeze_start_s = if let Some(&first_pre_roll) = pre_roll_frames.first() {
        (options.start_ns as i64 - first_pre_roll) as f64 / 1_000_000_000.0
    } else {
        0.0
    };
    
    let squeeze_end_s = if let (Some(end), Some(&last_post_roll)) = (options.end_ns, post_roll_frames.last()) {
        (last_post_roll - end as i64) as f64 / 1_000_000_000.0
    } else {
        0.0
    };

    // Second pass: remux
    input_file.seek(SeekFrom::Start(best_start_pos))?;
    let mut post_roll_index = 0u64;
    let mut should_stop = false;

    while !should_stop {
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break,
        };
        if header.id == Cluster::ID {
            let mut cluster = Cluster::read_element(&header, &mut input_file)?;
            let orig_cluster_ticks = cluster.timestamp.0;
            
            cluster.timestamp.0 = cluster.timestamp.0.saturating_sub(best_start_cluster_ticks);
            let shifted_cluster_ticks = cluster.timestamp.0 as i64;

            let mut filtered = Vec::new();
            
            for block_enum in cluster.blocks {
                if let ClusterBlock::Simple(mut sb) = block_enum {
                    let data = &mut sb.0;
                    if data.len() < 4 {
                        continue;
                    }
                    let mut cursor = std::io::Cursor::new(&data[..]);
                    let track_num = VInt64::read_from(&mut cursor)
                        .unwrap_or(VInt64::new(0))
                        .value;
                    
                    // Filter by track
                    if let Some(ref filter) = options.track_filter {
                        if !filter.contains(&track_num) {
                            continue;
                        }
                    }
                    
                    let kind = track_map.get(&track_num).unwrap_or(&TrackKind::Other);

                    let abs_ns = data.get_timestamp(orig_cluster_ticks as i64, timecode_scale)
                        .unwrap_or(0);

                    if *kind == TrackKind::Audio {
                        if abs_ns < options.start_ns as i64 {
                            continue;
                        }
                        
                        if let Some(end) = options.end_ns {
                            if abs_ns > end as i64 {
                                continue;
                            }
                        }
                        
                        let audio_offset_from_start_ns = abs_ns - options.start_ns as i64;
                        let new_audio_ns = squeeze_start_ns as i64 + audio_offset_from_start_ns;
                        data.set_timestamp(new_audio_ns, shifted_cluster_ticks, timecode_scale)?;
                    } 
                    else if *kind == TrackKind::Video {
                        if abs_ns < options.start_ns as i64 {
                            if pre_roll_count > 0 {
                                data.set_timestamp(0, shifted_cluster_ticks, timecode_scale)?;
                                data.set_invisible(true)?;
                            }
                        } else if let Some(end) = options.end_ns {
                            if abs_ns > end as i64 {
                                if post_roll_index >= post_roll_count as u64 {
                                    should_stop = true;
                                    continue;
                                }
                                
                                if post_roll_count > 0 {
                                    data.set_timestamp(0, shifted_cluster_ticks, timecode_scale)?;
                                    data.set_invisible(true)?;
                                    post_roll_index += 1;
                                } else {
                                    continue;
                                }
                            } else {
                                let offset_from_start_ns = abs_ns - options.start_ns as i64;
                                let new_video_ns = squeeze_start_ns as i64 + offset_from_start_ns;
                                data.set_timestamp(new_video_ns, shifted_cluster_ticks, timecode_scale)?;
                            }
                        } else {
                            let offset_from_start_ns = abs_ns - options.start_ns as i64;
                            let new_video_ns = squeeze_start_ns as i64 + offset_from_start_ns;
                            data.set_timestamp(new_video_ns, shifted_cluster_ticks, timecode_scale)?;
                        }
                    }
                    
                    if abs_ns >= (options.start_ns - squeeze_end_ns*2) as i64 && (options.end_ns.is_none() || abs_ns <= (options.end_ns.unwrap_or(0) + squeeze_end_ns*2) as i64) { 
                        filtered.push(ClusterBlock::Simple(sb));
                    }
                } else {
                    filtered.push(block_enum);
                }
            }
            
            cluster.blocks = filtered;
            cluster.write_to(&mut writer)?;
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            } else {
                break;
            }
        }
    }

    writer.flush()?;
    drop(writer);
    
    Ok(CutResult {
        start_offset_s: squeeze_start_s,
        end_offset_s: squeeze_end_s,
        data: Cursor::new(output),
    })
}
