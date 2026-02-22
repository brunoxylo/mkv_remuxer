use super::Sink;
use crate::{ClusterBlockExt, Result};
use log::{debug, warn};
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

const CUE_INTERVAL_NS: u64 = 15_000_000_000; // 15 seconds


/// File-based sink implementation for writing MKV files (legacy trait implementation)
pub struct FileSink {
    writer: BufWriter<File>,
    segment_started: bool,
    segment_start_offset: u64,
    cues_offset: u64,
    reserved_cues_size: u64,
    timescale: u64,
    cue_points: Vec<(u64, u64)>, // (timestamp_ns, cluster_file_position)
    last_cue_timestamp_ns: u64,
}

impl FileSink {
    /// Create a new file sink that writes to the specified path
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        Ok(Self {
            writer,
            segment_started: false,
            segment_start_offset: 0,
            cues_offset: 0,
            reserved_cues_size: 0,
            timescale: 1_000_000,
            cue_points: Vec::new(),
            last_cue_timestamp_ns: 0,
        })
    }
}

impl Sink for FileSink {
    fn initialize(
        &mut self,
        tracks: &Tracks,
        info: &Info,
        chapters: Option<&Chapters>,
    ) -> Result<()> {
        // Write EBML header
        let ebml_header = Ebml {
            ebml_version: Some(EbmlVersion(1)),
            ebml_read_version: Some(EbmlReadVersion(1)),
            ebml_max_id_length: EbmlMaxIdLength(4),
            ebml_max_size_length: EbmlMaxSizeLength(8),
            doc_type: Some(DocType("matroska".to_string())),
            doc_type_version: Some(DocTypeVersion(4)),
            doc_type_read_version: Some(DocTypeReadVersion(2)),
            crc32: None,
            void: None,
        };
        ebml_header.write_to(&mut self.writer)?;

        // Write Segment start with unknown size for streaming
        // Segment ID is 0x18538067
        self.writer.write_all(&[0x18, 0x53, 0x80, 0x67])?;
        self.segment_start_offset = self.writer.stream_position()?;

        // Unknown size marker (all 1s in VINT encoding)
        self.writer
            .write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;

        // Write Info element inside the segment
        info.write_to(&mut self.writer)?;

        // Write Tracks element inside the segment
        tracks.write_to(&mut self.writer)?;

        // Write Chapters element inside the segment (if present)
        if let Some(chapters) = chapters {
            chapters.write_to(&mut self.writer)?;
        }

        // Reserve space for Cues
        // Estimate number of clusters: duration / 5s
        let duration_ns = info
            .duration
            .map(|d| (d.0 * info.timestamp_scale.0 as f64) as u64)
            .unwrap_or(0);
        let num_clusters = (duration_ns / CUE_INTERVAL_NS).max(1);
        // Reserve ~64 bytes per cue point, plus some safety 
        let estimated_cues_size = num_clusters * 25 + 1024;
        println!("Reserving {} bytes for cues (estimated {} cue points, total duration {} ns)", estimated_cues_size, num_clusters, duration_ns);

        self.cues_offset = self.writer.stream_position()?;
        
        let void = Void {
            size: estimated_cues_size,
        };
        void.write_to(&mut self.writer)?;
        
        // Record the ACTUAL total space occupied by the Void element (header + data)
        let end_of_reserved_space = self.writer.stream_position()?;
        self.reserved_cues_size = end_of_reserved_space - self.cues_offset;

        self.writer.flush()?;

        // Store timescale for timestamp calculations
        self.timescale = info.timestamp_scale.0;
        self.segment_started = true;
        Ok(())
    }

    fn write_cluster(&mut self, cluster: &Cluster, _track_number: u64) -> Result<()> {
        if !self.segment_started {
            return Err(crate::Error::InvalidConfig(
                "Cannot write cluster before initialize() is called".to_string(),
            ));
        }

        // Get cluster position before writing
        let cluster_position = self.writer.stream_position()?;
        
        // Calculate cluster timestamp in nanoseconds
        let cluster_timestamp_ticks = cluster.timestamp.0;
        let cluster_timestamp_ns = cluster_timestamp_ticks * self.timescale;
        
        // Add cue point if 15 seconds have passed since last cue
        if cluster_timestamp_ns >= self.last_cue_timestamp_ns + CUE_INTERVAL_NS || self.cue_points.is_empty() {
            self.cue_points.push((cluster_timestamp_ticks, cluster_position));
            self.last_cue_timestamp_ns = cluster_timestamp_ns;
        }
        
        cluster.write_to(&mut self.writer)?;
        self.writer.flush()?;
        print!("written cluster at position {}, timestamp {} ns", cluster_position, cluster_timestamp_ns);
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        // Generate and write cues from collected cue points
        if !self.cue_points.is_empty() && self.cues_offset > 0 {
            let segment_data_start = self.segment_start_offset + 8;
            
            let mut cues = Cues {
                crc32: None,
                cue_point: Vec::new(),
                void: None,
            };
            
            for (timestamp_ticks, cluster_position) in &self.cue_points {
                // Calculate position relative to Segment data start
                let relative_position = cluster_position - segment_data_start;
                
                let cue_point = CuePoint {
                    crc32: None,
                    cue_time: CueTime(*timestamp_ticks),
                    cue_track_positions: vec![CueTrackPositions {
                        cue_track: CueTrack(1),
                        cue_cluster_position: CueClusterPosition(relative_position),
                        cue_relative_position: None,
                        cue_duration: None,
                        cue_block_number: None,
                        cue_codec_state: CueCodecState(0),
                        cue_reference: Vec::new(),
                        void: None,
                        crc32: None,
                    }],
                    void: None,
                };
                cues.cue_point.push(cue_point);
            }
            
            // Record current position to seek back later
            self.writer.flush()?;
            let current_pos = self.writer.stream_position()?;
            
            // Seek to the reserved space
            self.writer.seek(SeekFrom::Start(self.cues_offset))?;
            
            // Try to serialize cues and truncate if necessary
            let mut cues_to_write = cues;
            let mut cues_buf = Vec::new();
            cues_to_write.write_to(&mut cues_buf)?;
            
            // If cues are too large, truncate cue points until they fit
            if cues_buf.len() as u64 > self.reserved_cues_size {
                let original_count = cues_to_write.cue_point.len();
                
                // Iteratively remove cue points from the end until it fits
                while cues_buf.len() as u64 > self.reserved_cues_size && !cues_to_write.cue_point.is_empty() {
                    cues_to_write.cue_point.pop();
                    cues_buf.clear();
                    cues_to_write.write_to(&mut cues_buf)?;
                }

                
                let kept_count = cues_to_write.cue_point.len();
                let removed_count = original_count - kept_count;
                
                if removed_count > 0 {
                    warn!(
                        "Truncated cues: removed {} of {} cue points to fit in reserved space ({} bytes). Final size: {} bytes",
                        removed_count,
                        original_count,
                        self.reserved_cues_size,
                        cues_buf.len()
                    );
                }
                
                // If even an empty cues structure doesn't fit, we have a problem
                if cues_buf.len() as u64 > self.reserved_cues_size {
                    return Err(crate::Error::InvalidConfig(format!(
                        "Reserved cues space ({} bytes) is too small even for empty cues structure ({} bytes)",
                        self.reserved_cues_size,
                        cues_buf.len()
                    )));
                }
            }
            println!("cue buf size after truncation: {}", cues_buf.len());

            
            self.writer.write_all(&cues_buf)?;
            
            // If we have remaining reserved space, fill it with Void
            if (cues_buf.len() as u64) < self.reserved_cues_size {
                let remaining = self.reserved_cues_size - cues_buf.len() as u64;
                
                // Calculate correct Void payload size by trial
                // We need: void_header_size + payload_size = remaining
                // Start with remaining-2 and adjust if header size would be bigger
                let mut void_payload_size = remaining.saturating_sub(2);
                
                // Test if this size fits by serializing to a temp buffer
                loop {
                    let test_void = Void {
                        size: void_payload_size,
                    };
                    let mut temp_buf = Vec::new();
                    if test_void.write_to(&mut temp_buf).is_ok() {
                        if temp_buf.len() as u64 <= remaining {
                            // This size works
                            self.writer.write_all(&temp_buf)?;
                            
                            // Pad any remaining bytes with zeros
                            let pad_bytes = remaining - temp_buf.len() as u64;
                            if pad_bytes > 0 {
                                self.writer.write_all(&vec![0u8; pad_bytes as usize])?;
                            }
                            break;
                        }
                    }
                    
                    // If we get here, the Void was too large. Reduce payload and try again.
                    if void_payload_size == 0 {
                        // Can't fit a Void at all, just pad with zeros
                        self.writer.write_all(&vec![0u8; remaining as usize])?;
                        break;
                    }
                    void_payload_size = void_payload_size.saturating_sub(1);
                }
            }
            
            // Seek back to the original position
            self.writer.flush()?;
            self.writer.seek(SeekFrom::Start(current_pos))?;
        }
        
        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mkv_element::prelude::Cues;
    use std::io::Read;

    #[test]
    fn test_file_sink_cues_reservation() -> Result<()> {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_cues_{}.mkv", uuid::Uuid::new_v4()));

        {
            let mut sink = FileSink::new(&path)?;

            let tracks = Tracks {
                track_entry: vec![TrackEntry {
                    track_number: TrackNumber(1),
                    track_uid: TrackUid(123),
                    track_type: TrackType(1),
                    codec_id: CodecId("V_VP8".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            };

            let info = Info {
                timestamp_scale: TimestampScale(1_000_000),
                muxing_app: MuxingApp("test".to_string()),
                writing_app: WritingApp("test".to_string()),
                duration: Some(Duration(10000.0)), // 10s
                ..Default::default()
            };

            sink.initialize(&tracks, &info, None)?;

            assert!(sink.cues_offset > 0);
            assert!(sink.reserved_cues_size > 1024);

            let cues = Cues {
                cue_point: vec![CuePoint {
                    cue_time: CueTime(0),
                    cue_track_positions: vec![CueTrackPositions {
                        cue_track: CueTrack(1),
                        cue_cluster_position: CueClusterPosition(sink.cues_offset),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            };

            sink.finalize()?;
        }

        // Basic verification: file exists and has some content
        assert!(path.exists());
        let mut file = File::open(&path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        assert!(buf.len() > 100);

        // Cleanup
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}
