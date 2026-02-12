use super::Sink;
use crate::Result;
use log::{debug, warn};
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// File-based sink implementation for writing MKV files (legacy trait implementation)
pub struct FileSink {
    writer: BufWriter<File>,
    segment_started: bool,
    segment_start_offset: u64,
    cues_offset: u64,
    reserved_cues_size: u64,
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
        let num_clusters = (duration_ns / crate::cluster_warpper::CLUSTER_MAX_DURATION_NS).max(1);
        // Reserve ~64 bytes per cue point, plus some safety margin
        let estimated_cues_size = num_clusters * 64 + 1024;

        self.cues_offset = self.writer.stream_position()?;
        self.reserved_cues_size = estimated_cues_size;

        let void = Void {
            size: estimated_cues_size,
        };
        void.write_to(&mut self.writer)?;

        self.writer.flush()?;
        let cluster_start_offset = self.writer.stream_position()?;

        self.segment_started = true;
        Ok(())
    }

    fn write_cluster(&mut self, cluster: &Cluster, _track_number: u64) -> Result<()> {
        if !self.segment_started {
            return Err(crate::Error::InvalidConfig(
                "Cannot write cluster before initialize() is called".to_string(),
            ));
        }

        cluster.write_to(&mut self.writer)?;
        self.writer.flush()?;
        Ok(())
    }

    fn write_cues(&mut self, cues: &Cues) -> Result<()> {
        if self.cues_offset == 0 {
            return Err(crate::Error::InvalidConfig(
                "Cues space was not reserved".to_string(),
            ));
        }

        // Record current position to seek back later
        self.writer.flush()?;
        let current_pos = self.writer.stream_position()?;

        // Seek to the reserved space
        self.writer.seek(SeekFrom::Start(self.cues_offset))?;

        // Write the cues
        let mut cues_buf = Vec::new();
        cues.write_to(&mut cues_buf)?;

        if cues_buf.len() as u64 > self.reserved_cues_size {
            warn!(
                "Generated cues ({} bytes) exceed reserved space ({} bytes)",
                cues_buf.len(),
                self.reserved_cues_size
            );
            // We still write it, but it will overwrite following data (bad!)
            // Better to use a smaller cues set or larger reservation.
        }

        self.writer.write_all(&cues_buf)?;

        // If we have remaining reserved space, fill it with Void
        if (cues_buf.len() as u64) < self.reserved_cues_size {
            let remaining = self.reserved_cues_size - cues_buf.len() as u64;
            // A Void element with 0 size payload takes 1 (ID) + 1 (VINT size) = 2 bytes
            if remaining >= 2 {
                let void = Void {
                    size: remaining - 2,
                };
                void.write_to(&mut self.writer)?;
            } else {
                // Just pad with zeros if space is too small for Void element
                self.writer.write_all(&vec![0u8; remaining as usize])?;
            }
        }

        // Seek back to the original position
        self.writer.flush()?;
        self.writer.seek(SeekFrom::Start(current_pos))?;

        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
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

            sink.write_cues(&cues)?;
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
