use anyhow::{Context, Result};
use clap::Parser;
use duration_str::parse;
use mkv_element::ClusterBlock;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input MKV/WebM file
    #[arg(short, long)]
    input: String,

    /// Start time (e.g., "10s", "00:00:10")
    #[arg(short, long)]
    start: String,

    /// End time (e.g., "20s", "00:01:00")
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
    } // invalid
}

fn main() -> Result<()> {
    let args = Args::parse();
    let start_duration = parse(&args.start)?;
    let start_ns = start_duration.as_nanos() as u64;
    let end_ns = args
        .end
        .as_deref()
        .map(parse)
        .transpose()?
        .map(|d| d.as_nanos() as u64);

    let mut input_file = File::open(&args.input).context("Failed to open input file")?;
    let output = std::io::stdout();
    let mut writer = BufWriter::new(output.lock());

    // 1. Read EBML Header
    let ebml_header = Header::read_from(&mut input_file).context("Failed to read EBML header")?;
    if ebml_header.id.value != Ebml::ID.value {
        anyhow::bail!("Not an EBML file");
    }
    let ebml =
        Ebml::read_element(&ebml_header, &mut input_file).context("Failed to parse EBML body")?;
    ebml.write_to(&mut writer).context("Failed to write EBML")?;

    // 2. Read Segment Header
    let segment_header =
        Header::read_from(&mut input_file).context("Failed to read Segment header")?;
    if segment_header.id.value != Segment::ID.value {
        anyhow::bail!(
            "Expected Segment element, found ID: {:x}",
            segment_header.id.value
        );
    }

    // Write Segment Header
    let mut out_segment_header = segment_header.clone();
    out_segment_header.size = VInt64::new_unknown();
    out_segment_header
        .write_to(&mut writer)
        .context("Failed to write Segment header")?;

    // State
    let mut timecode_scale = 1_000_000; // Default 1ms
    let mut keep_clusters = false;
    let mut start_offset_ticks: Option<u64> = None;

    let mut position = input_file.stream_position()?;
    let file_len = input_file.metadata()?.len();

    while position < file_len {
        let header = match Header::read_from(&mut input_file) {
            Ok(h) => h,
            Err(_) => break, // EOF likely
        };

        if header.id == Info::ID {
            let mut info = Info::read_element(&header, &mut input_file)?;
            timecode_scale = info.timestamp_scale.0;

            // Update Duration
            let new_duration_ns = if let Some(end) = end_ns {
                end.saturating_sub(start_ns)
            } else if let Some(old_duration) = &info.duration {
                let old_duration_ns = (old_duration.0 * timecode_scale as f64) as u64;
                old_duration_ns.saturating_sub(start_ns)
            } else {
                0
            };

            if new_duration_ns > 0 {
                info.duration = Some(Duration(new_duration_ns as f64 / timecode_scale as f64));
            }

            info.write_to(&mut writer)?;
        } else if header.id == Tracks::ID {
            let tracks = Tracks::read_element(&header, &mut input_file)?;
            tracks.write_to(&mut writer)?;
        } else if header.id == Cluster::ID {
            let mut cluster = Cluster::read_element(&header, &mut input_file)?;

            let cluster_time_scaled = cluster.timestamp.0;
            let cluster_time_ns = cluster_time_scaled as u64 * timecode_scale;

            if let Some(end) = end_ns {
                if cluster_time_ns > end {
                    break;
                }
            }

            // Filter Blocks and mark invisible
            for block_enum in &mut cluster.blocks {
                if let ClusterBlock::Simple(simple_block) = block_enum {
                    let data = &mut simple_block.0;
                    if data.len() < 4 {
                        continue;
                    }

                    let track_len = vint_length(data[0]);
                    if track_len == 0 || data.len() < track_len + 3 {
                        continue;
                    }

                    let timecode_idx = track_len;
                    let flags_idx = track_len + 2;

                    let tc_bytes = [data[timecode_idx], data[timecode_idx + 1]];
                    let timecode = i16::from_be_bytes(tc_bytes);

                    let block_time_ns =
                        (cluster_time_scaled as i64 + timecode as i64) as u64 * timecode_scale;

                    if let Some(end) = end_ns {
                        if block_time_ns > end {
                            continue;
                        }
                    }

                    if block_time_ns < start_ns {
                        // Mark as invisible (Bit 3: 0x08)
                        data[flags_idx] |= 0x08;
                    } else {
                        keep_clusters = true;
                    }
                }
            }

            if keep_clusters {
                // Initialize offset with the first kept cluster's timestamp
                let offset = *start_offset_ticks.get_or_insert(cluster_time_scaled);

                // Shift cluster timestamp.
                // Since this cluster is kept, its timestamp must be >= offset (because clusters are ordered)
                // The first kept cluster will have timestamp == offset, so it becomes 0.
                if cluster.timestamp.0 >= offset {
                    cluster.timestamp.0 -= offset;
                } else {
                    // Should theoretically not happen given strictly increasing clusters,
                    // but just in case, clamp to 0
                    cluster.timestamp.0 = 0;
                }

                cluster.write_to(&mut writer)?;
            }
        } else {
            // header.size is VInt64
            let size = header.size.value;
            if size > 0 && !header.size.is_unknown {
                input_file.seek(SeekFrom::Current(size as i64))?;
            }
        }

        position = input_file.stream_position()?;
    }

    Ok(())
}
