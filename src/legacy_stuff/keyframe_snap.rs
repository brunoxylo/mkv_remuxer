// Keyframe snapping approach: Cut at the nearest keyframe boundaries.
// Enables fast seeking since we start exactly at a keyframe, but may not
// match the exact requested start/end times.

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

    // 4. Build track map
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

    // 5. Find nearest keyframe cluster to start
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
                if cluster_time_ns <= start_ns {
                    keyframe_before_pos = pos;
                    keyframe_before_time_ns = cluster_time_ns;
                    keyframe_before_ticks = cluster.timestamp.0;
                } else {
                    // Found keyframe after start - compare distances
                    let dist_before = start_ns.saturating_sub(keyframe_before_time_ns);
                    let dist_after = cluster_time_ns.saturating_sub(start_ns);
                    
                    if dist_after < dist_before {
                        // Keyframe after is closer
                        start_keyframe_pos = pos;
                        start_keyframe_time_ns = cluster_time_ns;
                        start_cluster_ticks = cluster.timestamp.0;
                    } else {
                        // Keyframe before is closer or equal
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
    
    // If we never found a keyframe after start, use the last one before
    if start_keyframe_pos == clusters_start_pos && keyframe_before_pos != clusters_start_pos {
        start_keyframe_pos = keyframe_before_pos;
        start_keyframe_time_ns = keyframe_before_time_ns;
        start_cluster_ticks = keyframe_before_ticks;
    }

    // 6. Find nearest keyframe cluster to end (if end is specified)
    let mut end_keyframe_time_ns = None;
    if let Some(end) = end_ns {
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
                        // Found keyframe after end - compare distances
                        if let Some(before_ns) = keyframe_before_end_time_ns {
                            let dist_before = end.saturating_sub(before_ns);
                            let dist_after = cluster_time_ns.saturating_sub(end);
                            
                            if dist_after < dist_before {
                                // Keyframe after is closer
                                end_keyframe_time_ns = Some(cluster_time_ns);
                            } else {
                                // Keyframe before is closer or equal
                                end_keyframe_time_ns = Some(before_ns);
                            }
                        } else {
                            // No keyframe before, use this one
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
        
        // If we never found a keyframe after end, use the last one before
        if end_keyframe_time_ns.is_none() {
            end_keyframe_time_ns = keyframe_before_end_time_ns;
        }
    }

    eprintln!("Snapping to keyframe at {}s (requested {}s)", 
              start_keyframe_time_ns as f64 / 1_000_000_000.0,
              start_ns as f64 / 1_000_000_000.0);
    
    if let Some(end_kf_ns) = end_keyframe_time_ns {
        eprintln!("Snapping to keyframe at {}s (requested {}s)",
                  end_kf_ns as f64 / 1_000_000_000.0,
                  end_ns.unwrap_or(0) as f64 / 1_000_000_000.0);
    }

    // 7. Write modified headers
    if let Some(mut info) = stored_info {
        if let Some(end_kf_ns) = end_keyframe_time_ns {
            let duration_ns = end_kf_ns.saturating_sub(start_keyframe_time_ns);
            info.duration = Some(Duration(duration_ns as f64 / timecode_scale as f64));
        }
        info.write_to(&mut writer)?;
    }
    
    if let Some(tracks) = stored_tracks {
        tracks.write_to(&mut writer)?;
    }

    // 8. Copy clusters from start keyframe to end keyframe
    input_file.seek(SeekFrom::Start(start_keyframe_pos))?;

    loop {
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break,
        };
        if header.id == Cluster::ID {
            let mut cluster = Cluster::read_element(&header, &mut input_file)?;
            let cluster_time_ns = cluster.timestamp.0 * timecode_scale;
            
            // Stop if we've reached the end keyframe
            if let Some(end_kf_ns) = end_keyframe_time_ns {
                if cluster_time_ns >= end_kf_ns {
                    break;
                }
            }
            
            // Shift cluster timestamp to start from 0
            cluster.timestamp.0 = cluster.timestamp.0.saturating_sub(start_cluster_ticks);
            
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
