// Squeeze approach: Pre-roll frames are compressed into first few milliseconds,
// post-roll frames compressed into last few milliseconds. This maintains decoder state
// while making unwanted frames flash by too fast to see.

use anyhow::{Context, Result};
use mkv_element::ClusterBlock;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};

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
    /// Get absolute timestamp in nanoseconds from a SimpleBlock (can be negative)
    fn get_timestamp(&self, cluster_timestamp: i64, timecode_scale: u64) -> Option<i64>;
    
    /// Check if the invisible flag is set
    fn is_invisible(&self) -> Option<bool>;
}

/// Extension trait for modifying SimpleBlock data
trait SimpleBlockExtMut {
    /// Set timestamp from absolute time in nanoseconds (can be negative)
    fn set_timestamp(&mut self, time_in_ns: i64, cluster_timestamp: i64, timecode_scale: u64) -> Result<()>;
    
    /// Set or clear the invisible flag
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
            self[track_len + 2] |= 0x08;  // Set bit 4
        } else {
            self[track_len + 2] &= !0x08; // Clear bit 4
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

pub fn process(input_path: &str, start_ns: u64, end_ns: Option<u64>) -> Result<()> {
    let mut input_file = File::open(input_path).context("Failed to open input file")?;
    let mut writer = BufWriter::new(std::io::stdout().lock());

    // 1. EBML Header
    let ebml_header = Header::read_from(&mut input_file).context("Failed to read EBML header")?;
    let ebml =
        Ebml::read_element(&ebml_header, &mut input_file).context("Failed to read EBML body")?;
    ebml.write_to(&mut writer).context("Failed to write EBML")?;

    // 2. Segment Header
    let segment_header =
        Header::read_from(&mut input_file).context("Failed to read Segment header")?;
    if segment_header.id.value != Segment::ID.value {
        anyhow::bail!("Expected Segment, found ID: {:x}", segment_header.id.value);
    }
    let mut out_segment_header = segment_header.clone();
    out_segment_header.size = VInt64::new_unknown();
    out_segment_header
        .write_to(&mut writer)
        .context("Failed to write Segment header")?;

    // 3. Scan headers and find Clusters start
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

    // 4. Find best keyframe cluster before start
    input_file.seek(SeekFrom::Start(clusters_start_pos))?;
    let mut best_start_pos = clusters_start_pos;
    let mut best_start_cluster_ticks = 0u64;
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
                if cluster_time_ns <= start_ns {
                    best_start_pos = pos;
                    best_start_cluster_ticks = cluster.timestamp.0;
                } else {
                    break;
                }
            } else if cluster_time_ns > start_ns + 50_000_000_000 {
                break;
            }
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    // Squeeze windows
    let squeeze_start_ns = 10_000_000u64; // 100ms window for pre-roll
    let squeeze_end_ns = 10_000_000u64;   // 100ms window for post-roll

    // 5. Write modified headers
    if let Some(mut info) = stored_info {
        if let Some(end) = end_ns {
            // Duration = squeeze window + actual content duration
            let content_dur_ns = end.saturating_sub(start_ns);
            let total_dur_ns = squeeze_start_ns + content_dur_ns;
            info.duration = Some(Duration(total_dur_ns as f64 / timecode_scale as f64));
        }
        info.write_to(&mut writer)?;
    }
    
    if let Some(tracks) = stored_tracks {
        // No codec_delay needed since we're physically adjusting timestamps
        tracks.write_to(&mut writer)?;
    }

    // 6. First pass: collect all video frames to count pre-roll and post-roll
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
                        
                        if let Some(end) = end_ns {
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
    
    // Count pre-roll and post-roll frames
    let pre_roll_count = video_frames.iter().filter(|&&ts| ts < start_ns as i64).count();
    let post_roll_count = end_ns.map(|end| video_frames.iter().filter(|&&ts| ts > end as i64).count()).unwrap_or(0);
    
    eprintln!("Pre-roll frames: {}, will squeeze into timestamp 0", pre_roll_count);
    if post_roll_count > 0 {
        eprintln!("Post-roll frames: {}, will squeeze into last timestamp", post_roll_count);
    }

    // 7. Second pass: Remux with squeezed timestamps
    input_file.seek(SeekFrom::Start(best_start_pos))?;
    let mut pre_roll_index = 0u64;
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
            
            // Shift cluster timestamp
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
                    let kind = track_map.get(&track_num).unwrap_or(&TrackKind::Other);

                    let abs_ns = data.get_timestamp(orig_cluster_ticks as i64, timecode_scale)
                        .unwrap_or(0);

                    if *kind == TrackKind::Audio {
                        // Drop audio before start (pre-roll is video-only for decoder state)
                        if abs_ns < start_ns as i64 {
                            continue;
                        }
                        
                        // Trim audio after end
                        if let Some(end) = end_ns {
                            if abs_ns > end as i64 {
                                continue;
                            }
                        }
                        
                        // Shift audio so it starts right after the video pre-roll squeeze window
                        // Audio at start_ns should appear at squeeze_start_ns (100ms) in output
                        let audio_offset_from_start_ns = abs_ns - start_ns as i64;
                        let new_audio_ns = squeeze_start_ns as i64 + audio_offset_from_start_ns;
                        data.set_timestamp(new_audio_ns, shifted_cluster_ticks, timecode_scale)?;
                    } 
                    else if *kind == TrackKind::Video {
                        if abs_ns < start_ns as i64 {
                            // Pre-roll frame: set to timestamp 0 and mark invisible
                            if pre_roll_count > 0 {
                                data.set_timestamp(0, shifted_cluster_ticks, timecode_scale)?;
                                data.set_invisible(true)?;
                                pre_roll_index += 1;
                            }
                        } else if let Some(end) = end_ns {
                            if abs_ns > end as i64 {
                                // Check if we've already processed all post-roll frames
                                if post_roll_index >= post_roll_count as u64 {
                                    // Skip this frame and stop processing cluster
                                    should_stop = true;
                                    continue;  // Don't add to filtered
                                }
                                
                                // Post-roll frame: set to timestamp 0 and mark invisible
                                if post_roll_count > 0 {
                                    data.set_timestamp(end as i64 - start_ns as i64, shifted_cluster_ticks, timecode_scale)?;
                                    data.set_invisible(true)?;
                                    post_roll_index += 1;
                                } else {
                                    continue;
                                }
                            } else {
                                // Main content frame: shift forward by squeeze_start_ns
                                let offset_from_start_ns = abs_ns - start_ns as i64;
                                let new_video_ns = squeeze_start_ns as i64 + offset_from_start_ns;
                                data.set_timestamp(new_video_ns, shifted_cluster_ticks, timecode_scale)?;
                            }
                        } else {
                            // No end specified: shift main content forward by squeeze_start_ns
                            let offset_from_start_ns = abs_ns - start_ns as i64;
                            let new_video_ns = squeeze_start_ns as i64 + offset_from_start_ns;
                            data.set_timestamp(new_video_ns, shifted_cluster_ticks, timecode_scale)?;
                        }
                    }
                    // only output frames that are within the pre-roll or post-roll squeeze windows, or the main content
                    let final_block_time_ns = data.get_timestamp(shifted_cluster_ticks, timecode_scale).unwrap_or(0);
                    let duration = if let Some(end) = end_ns {
                        Some(end as i64 - start_ns as i64)   
                    } else {
                        None
                    };
                    if duration.is_none() || final_block_time_ns <= (duration.unwrap_or(0) + squeeze_end_ns as i64) { 
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
    Ok(())
}
