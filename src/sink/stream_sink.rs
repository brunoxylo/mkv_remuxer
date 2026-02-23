use super::Sink;
use crate::Result;
use log::trace;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::io::{Seek, Write};

/// Stream-based sink implementation for writing MKV files to any stream
/// 
/// This sink writes to any `Write + Seek` stream without managing cues,
/// since seekable streams allow the consumer to navigate the file.
pub struct StreamSink<W: Write + Seek + Send> {
    writer: W,
    segment_started: bool,
    segment_start_offset: u64,
    timescale: u64,
}

impl<W: Write + Seek + Send> StreamSink<W> {
    /// Create a new stream sink that writes to the specified stream
    pub fn new(writer: W) -> Result<Self> {
        Ok(Self {
            writer,
            segment_started: false,
            segment_start_offset: 0,
            timescale: 1_000_000,
        })
    }
}

impl<W: Write + Seek + Send> Sink for StreamSink<W> {
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
        
        cluster.write_to(&mut self.writer)?;
        trace!("written cluster at position {}, timestamp {} ns", cluster_position, cluster_timestamp_ns);
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        // No cues management needed for stream sink
        // Since the stream is seekable, consumers can navigate the file
        // without cue points
        
        // Just flush any remaining buffered data
        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_stream_sink_basic() -> Result<()> {
        let mut buffer = Cursor::new(Vec::new());
        let mut sink = StreamSink::new(&mut buffer)?;

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
        assert!(sink.segment_started);

        // Write a simple cluster
        let cluster = Cluster {
            timestamp: Timestamp(0),
            blocks: Vec::new(),
            crc32: None,
            void: None,
            position: None,
            prev_size: None,
        };

        sink.write_cluster(&cluster, 1)?;
        sink.finalize()?;

        // Verify buffer has content
        let output = buffer.into_inner();
        assert!(output.len() > 100);

        Ok(())
    }

    #[test]
    fn test_stream_sink_no_cues() -> Result<()> {
        // Verify that the output doesn't contain cues
        let mut buffer = Cursor::new(Vec::new());
        let mut sink = StreamSink::new(&mut buffer)?;

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
            duration: Some(Duration(60000.0)), // 60s - enough to trigger cues in FileSink
            ..Default::default()
        };

        sink.initialize(&tracks, &info, None)?;
        
        // Write several clusters to simulate a longer file
        for i in 0..10 {
            let cluster = Cluster {
                timestamp: Timestamp(i * 5000), // 5s intervals
                blocks: Vec::new(),
                crc32: None,
                void: None,
                position: None,
                prev_size: None,
            };
            sink.write_cluster(&cluster, 1)?;
        }
        
        sink.finalize()?;

        let output = buffer.into_inner();
        
        // The Cues element ID is 0x1C53BB6B
        // Make sure it doesn't appear in the output
        let cues_id = vec![0x1C, 0x53, 0xBB, 0x6B];
        let has_cues = output.windows(4).any(|window| window == cues_id.as_slice());
        assert!(!has_cues, "StreamSink should not write cues");

        Ok(())
    }
}
