use std::io::Write;
use std::sync::mpsc;

use super::Sink;
use crate::sink::{ChannelWriterWrapper};
use crate::{ContainerFormat, Error, Result};
use log::trace;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;

/// Stream-based sink implementation for writing MKV files to any stream
/// 
/// This sink writes to any `Write + Seek` stream without managing cues,
/// since seekable streams allow the consumer to navigate the file.
pub struct StreamSink<W: Write + Send> {
    writer: W,
    timescale: u64,
}

impl<W: Write + Send> StreamSink<W> {

    /// Create a new stream sink that writes to the specified stream
    pub fn new(writer: W) -> Result<Self> {
        Ok(Self {
            writer,
            timescale: 1_000_000,
        })
    }
}

impl<W: Write + Send> Sink for StreamSink<W> {
    fn initialize(
        &mut self,
        tracks: &Tracks,
        info: &Info,
        ebml_header: &Ebml,
        chapters: Option<&Chapters>,
    ) -> Result<()> {

        match ebml_header.doc_type {
            Some(DocType(ref doc_type)) if doc_type.to_lowercase() == ContainerFormat::Mkv.to_string() => {},
            Some(DocType(ref doc_type)) if doc_type.to_lowercase() == ContainerFormat::WebM.to_string() => {},
            _ => {
                return Err(Error::InvalidConfig(format!(
                    "EBML header doc type must be mkv or webm for StreamSink", 
                )));
            }
        }
        // Write EBML header
        ebml_header.write_to(&mut self.writer)?;

        // Write Segment start with unknown size for streaming
        // Segment ID is 0x18538067
        self.writer.write_all(&[0x18, 0x53, 0x80, 0x67])?;

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
        self.writer.flush()?;

        // Store timescale for timestamp calculations
        self.timescale = info.timestamp_scale.0;
        Ok(())
    }

    fn write_cluster(&mut self, cluster: &Cluster, _track_number: u64) -> Result<()> {
        
        // Calculate cluster timestamp in nanoseconds
        let cluster_timestamp_ticks = cluster.timestamp.0;
        let cluster_timestamp_ns = cluster_timestamp_ticks * self.timescale;
        
        cluster.write_to(&mut self.writer)?;
        trace!("written cluster at timestamp {} ns", cluster_timestamp_ns);
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

    fn does_support_container_format(&self, format: ContainerFormat) -> bool {
        match format {
            ContainerFormat::Mkv => true,
            ContainerFormat::WebM => true,
            ContainerFormat::Vtt => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::sink::SinkSender;

    use super::*;

    fn collect_output(rx: std::sync::mpsc::Receiver<bytes::Bytes>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(chunk) = rx.recv() {
            out.extend_from_slice(&chunk);
        }
        out
    }

    fn make_ebml_header(doc_type: &str) -> Ebml {
        Ebml {
            ebml_version: Some(EbmlVersion(1)),
            ebml_read_version: Some(EbmlReadVersion(1)),
            ebml_max_id_length: EbmlMaxIdLength(4),
            ebml_max_size_length: EbmlMaxSizeLength(8),
            doc_type: Some(DocType(doc_type.to_string())),
            doc_type_version: Some(DocTypeVersion(4)),
            doc_type_read_version: Some(DocTypeReadVersion(2)),
            crc32: None,
            void: None,
        }
    }

    fn make_tracks() -> Tracks {
        Tracks {
            track_entry: vec![TrackEntry {
                track_number: TrackNumber(1),
                track_uid: TrackUid(123),
                track_type: TrackType(1),
                codec_id: CodecId("V_VP8".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn test_stream_sink_basic() -> Result<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel(100);
        let mut sink = StreamSink::new(ChannelWriterWrapper::new(SinkSender::Sync(tx)))?;

        let tracks = make_tracks();
        let info = Info {
            timestamp_scale: TimestampScale(1_000_000),
            muxing_app: MuxingApp("test".to_string()),
            writing_app: WritingApp("test".to_string()),
            duration: Some(Duration(10000.0)), // 10s
            ..Default::default()
        };

        sink.initialize(&tracks, &info, &make_ebml_header("matroska"), None)?;

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
        drop(sink); // closes the channel so collect_output terminates

        let output = collect_output(rx);
        assert!(output.len() > 100);

        Ok(())
    }

    #[test]
    fn test_stream_sink_no_cues() -> Result<()> {
        // Verify that the output doesn't contain cues
        let (tx, rx) = std::sync::mpsc::sync_channel(100);
        let mut sink = StreamSink::new(ChannelWriterWrapper::new(SinkSender::Sync(tx)))?;

        let info = Info {
            timestamp_scale: TimestampScale(1_000_000),
            muxing_app: MuxingApp("test".to_string()),
            writing_app: WritingApp("test".to_string()),
            duration: Some(Duration(60000.0)), // 60s - enough to trigger cues in FileSink
            ..Default::default()
        };

        sink.initialize(&make_tracks(), &info, &make_ebml_header("matroska"), None)?;

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
        drop(sink); // closes the channel so collect_output terminates

        let output = collect_output(rx);

        // The Cues element ID is 0x1C53BB6B
        // Make sure it doesn't appear in the output
        let cues_id = [0x1C, 0x53, 0xBB, 0x6B];
        let has_cues = output.windows(4).any(|window| window == cues_id);
        assert!(!has_cues, "StreamSink should not write cues");

        Ok(())
    }
}
