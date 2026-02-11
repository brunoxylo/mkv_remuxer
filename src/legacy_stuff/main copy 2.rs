use anyhow::{Context, Result};
use clap::Parser;
use duration_str::parse;
use mkv_element::ClusterBlock;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    input: String,
    #[arg(short, long)]
    start: String,
    #[arg(short, long)]
    end: Option<String>,
}

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

fn main() -> Result<()> {
    let args = Args::parse();
    let start_ns = parse(&args.start)?.as_nanos() as u64;
    let end_ns = args
        .end
        .as_deref()
        .map(parse)
        .transpose()?
        .map(|d| d.as_nanos() as u64);

    let mut input_file = File::open(&args.input).context("Failed to open input file")?;
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

    // 2. Scan Headers and find Clusters start
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

    // 3. Phase 1: Scan Clusters to find the Keyframe Cluster before Start
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
            } else if cluster_time_ns > start_ns + 5_000_000_000 {
                break;
            }
        } else {
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }

    // 4. Calculate Delay (Real Start vs Cluster Shift start)
    // Delay = Start - (Cluster_Start_Ticks * Scale)
    let delay_ns = start_ns.saturating_sub(best_start_cluster_ticks * timecode_scale);

    // 5. Write modified Headers
    if let Some(mut info) = stored_info {
        if let Some(end) = end_ns {
            let dur_ns = end.saturating_sub(start_ns);
            info.duration = Some(Duration(dur_ns as f64 / timecode_scale as f64));
        }
        info.write_to(&mut writer)?;
    }
    if let Some(mut tracks) = stored_tracks {
        for track in &mut tracks.track_entry {
            track.codec_delay = CodecDelay(delay_ns);
            // SeekPreRoll often goes with it
            track.seek_pre_roll = SeekPreRoll(delay_ns);
        }
        tracks.write_to(&mut writer)?;
    }

    // 6. Phase 2: Remux starting from best_start_pos
    input_file.seek(SeekFrom::Start(best_start_pos))?;
    let mut eos = false;

    while !eos {
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break,
        };
        if header.id == Cluster::ID {
            let mut cluster = Cluster::read_element(&header, &mut input_file)?;
            let orig_cluster_ticks = cluster.timestamp.0;
            // Shift Cluster Timestamp (always positive because >= best_start_cluster_ticks)
            cluster.timestamp.0 = cluster.timestamp.0.saturating_sub(best_start_cluster_ticks);

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
                    let kind = track_map.get(&track_num).unwrap_or(&TrackKind::Other);

                    let tc_bytes = [data[track_len], data[track_len + 1]];
                    let rel_ticks = i16::from_be_bytes(tc_bytes);
                    let abs_ns =
                        (orig_cluster_ticks as i64 + rel_ticks as i64) as u64 * timecode_scale;

                    if *kind == TrackKind::Audio {
                        if abs_ns < start_ns {
                            continue;
                        } // Dropping audio pre-roll
                        if let Some(end) = end_ns {
                            if abs_ns > end {
                                continue;
                            }
                        }
                    } else if *kind == TrackKind::Video {
                        if abs_ns < start_ns {
                            data[track_len + 2] |= 0x08; // Mark Invisible
                        } else if let Some(end) = end_ns {
                            if abs_ns > end {
                                if (data[track_len + 2] & 0x80) != 0 {
                                    eos = true;
                                    break;
                                }
                                data[track_len + 2] |= 0x08; // Mark Invisible
                            }
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
    Ok(())
}
