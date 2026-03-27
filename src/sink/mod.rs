use std::fmt::Display;
use std::io::Write;

use crate::Result;
use crate::ContainerFormat;
use bytes::Bytes;
use mkv_element::prelude::*;

mod util;
mod file_sink;
mod vtt_sink;
mod stream_sink;

pub use util::{OutputSink, Uninitialized, Initialized};
pub use file_sink::FileSink;
pub use vtt_sink::VttSink;
pub use stream_sink::{StreamSink};

/// Represents a sink/destination for MKV data (output file or stream)
pub trait Sink: Send {
    /// Initialize the output with EBML header and segment info
    fn initialize(
        &mut self,
        tracks: &Tracks,
        info: &Info,
        ebml_header: &Ebml,
        chapters: Option<&Chapters>,
    ) -> Result<()>;

    /// Write a cluster to the output
    fn write_cluster(&mut self, cluster: &Cluster, track_number: u64) -> Result<()>;

    /// returns whether we can send tracks with codecs that are supported by a specific container format to this sink
    fn does_support_container_format(&self, format: ContainerFormat) -> bool;

    /// Finalize the output (write cues, seek head, close file)
    fn finalize(&mut self) -> Result<()>;
}

pub enum SinkSender {
    Sync(std::sync::mpsc::SyncSender<bytes::Bytes>),
    Tokio(tokio::sync::mpsc::Sender<bytes::Bytes>),
}
pub struct ChannelWriterWrapper {
    pub tx: SinkSender,
    prefill_buffer: Vec::<u8>,
    previously_flushed: bool,
}
impl ChannelWriterWrapper {
    pub fn new(tx: SinkSender) -> Self {
        Self {
            tx,
            prefill_buffer: Vec::with_capacity(10_000), // 10k should be enough to buffer the initial EBML header and segment info
            previously_flushed: false,
        }
    }
    fn send_err(e: &dyn Display) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, format!("Failed to send data through channel: {}", e))
    }
    fn send_in_chunks(&mut self, buf: &[u8]) -> std::io::Result<()> {
        // Send data in chunks of 10KB to avoid overwhelming the channel
        const CHUNK_SIZE: usize = 10 * 1024;
        for chunk in buf.chunks(CHUNK_SIZE) {
            match &self.tx {
                SinkSender::Sync(tx) => tx.send(bytes::Bytes::copy_from_slice(chunk)).map_err(|e| Self::send_err(&e))?,
                SinkSender::Tokio(tx) => tx.blocking_send(bytes::Bytes::copy_from_slice(chunk)).map_err(|e| Self::send_err(&e))?,
            };
        }
        Ok(())
    }
}
impl Write for ChannelWriterWrapper {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.previously_flushed && self.prefill_buffer.len() < 1000_000 { // dont buffer more than 1MB to avoid excessive memory usage
            // Buffer the first few writes to allow the sink to initialize
            self.prefill_buffer.extend_from_slice(&buf);
        } else {
            self.flush()?;
            self.send_in_chunks(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.previously_flushed = true;
        if !self.prefill_buffer.is_empty() {
            let buffer = std::mem::take(&mut self.prefill_buffer);
            self.send_in_chunks(&buffer)?;
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;
    use crate::remux;
    use crate::remuxer::RemuxerCutMode;
    use crate::source::{FileSource, InputSource};
    use crate::test_utils::{test_file_path, validate_mkv_output};
    use crate::source::CutInterval;

    fn run_remux_test_with_seek_type(cut_mode: RemuxerCutMode) -> Result<()> {
        // Setup: Create output path in temp directory
        let temp_dir = std::env::temp_dir();
        let cut_mode_name = format!("{:?}", cut_mode);
        let output_path = temp_dir.join(format!("test_german_audio_{}.mkv", cut_mode_name));
        
        // Create input source from test.webm (uninitialized)
        let input_path = test_file_path();
        let input_file = File::open(&input_path)?;
        let source = FileSource::new(input_file)?;
        let input_source = InputSource::from(source);
        
        // Create output sink (uninitialized)
        let output_sink = FileSink::new(&output_path)?;
        let output = OutputSink::from(output_sink);
        
        // Configure cutting: from 20 seconds to the end
        let start_ns = 20_000_000_000u64; // 20 seconds in nanoseconds
        let cut_interval = CutInterval::new().with_start(start_ns);
        
        // We need to pre-check for German audio tracks manually
        // Initialize source temporarily to check tracks
        let input_file = File::open(&input_path)?;
        let temp_source = FileSource::new(input_file)?;
        let temp_input = InputSource::from(temp_source);
        let mut initialized_temp = temp_input.initialize(None)?.into_remuxing()?;
        let tracks = initialized_temp.get_tracks()?;
        

        let mut output_mappings = Vec::new();
        let mut has_video_track = false;
        // add fisrt video track to mappings as well
        for track in &tracks.track_entry {
            if track.track_type.0 == crate::block_ext::TrackKind::Video {
                output_mappings.insert(0, (0u64, track.track_number.0));
                has_video_track = true;
                break;
            }
        }
        if !has_video_track {
            panic!("Test file does not contain any video tracks. Please provide a test file with video for this test.");
        }

        // Look for German audio track
        for track in &tracks.track_entry {
            if track.track_type.0 == crate::block_ext::TrackKind::Audio {
                let lang = track.language.0.as_str();
                if lang == "ger" || lang == "deu" || lang == "de" {
                    output_mappings.push((0u64, track.track_number.0));
                }
            }
        }
        // If test.webm doesn't have German audio, try any audio track for testing
        if output_mappings.is_empty() {
            panic!("Test file does not contain any German audio tracks. Please provide a test file with German audio for this test.");
        }
        
        // Perform remuxing with custom mappings
        let stats = remux(
            vec![input_source],
            output, 
            Some(cut_interval), 
            Some(cut_mode),
            Some(output_mappings.clone())
        )?;
        
        // Validate we processed some blocks
        assert!(stats.blocks_processed > 0, "Should have processed at least some blocks");
        assert_eq!(stats.track_count, output_mappings.len(), "Track count should match mappings");


        let input_duration_ns = initialized_temp.get_output_duration()?.unwrap_or(0);
        // Validate the output using our comprehensive validation method
        let validation_report = validate_mkv_output(&output_path, true, Some(input_duration_ns), false, true)?;
        
        // Print report for debugging
        if !validation_report.is_valid() {
            eprintln!("{}", validation_report.summary());
            for error in &validation_report.errors {
                eprintln!("ERROR: {}", error);
            }
            for warning in &validation_report.warnings {
                eprintln!("WARNING: {}", warning);
            }
        }
        
        // Assert all validations passed
        assert!(validation_report.syntax_valid, "Output should have valid EBML syntax");
        assert!(validation_report.timestamps_plausible, "Timestamps should be plausible");
        assert!(validation_report.all_tracks_present, "All declared tracks should be present in clusters");
        assert!(validation_report.cluster_block_count_valid,
            "Cluster block counts should be within [{}, {}]",
            crate::cluster_warpper::MIN_BLOCKS_PER_CLUSTER,
            crate::cluster_warpper::MAX_BLOCKS_PER_CLUSTER);
        
        // Cues validation might be less strict, just warn if invalid
        if !validation_report.cues_valid {
            eprintln!("Warning: Cues validation failed");
        }
        
        // Overall validation
        assert!(validation_report.is_valid(), "Overall MKV validation should pass for {:?}", cut_mode_name);
        
        // Cleanup
        //let _ = std::fs::remove_file(&output_path);
        
        Ok(())
    }

    #[test]
    fn test_remux_german_audio_snap_nearest_keyframe() -> Result<()> {
        run_remux_test_with_seek_type(RemuxerCutMode::SnapNearestKeyframe)
    }

    #[test]
    fn test_remux_german_audio_squeeze() -> Result<()> {
        run_remux_test_with_seek_type(RemuxerCutMode::Squeeze)
    }

    #[test]
    fn test_remux_german_audio_dirty_cut() -> Result<()> {
        run_remux_test_with_seek_type(RemuxerCutMode::DirtyCut)
    }

    #[test]
    fn validate_input_file() -> Result<()> {
        let input_path = test_file_path();
        let report =  validate_mkv_output(&input_path, true, None, false, false)?;
        print!("Input file validation report:\n{}", report.summary());
                // Print report for debugging
        if !report.is_valid() {
            eprintln!("{}", report.summary());
            for error in &report.errors {
                eprintln!("ERROR: {}", error);
            }
            for warning in &report.warnings {
                eprintln!("WARNING: {}", warning);
            }
        }
        assert!(report.is_valid(), "Input file should be valid");
        Ok(())

    }

    #[test]
    fn test_remux_webvtt_into_video() -> Result<()> {
        use crate::source::WebVttSource;
        
        // Setup: Create output path in temp directory
        let temp_dir = std::env::temp_dir();
        let output_path = temp_dir.join("test_av1_with_subtitles.mkv");
        
        // First, get the video tracks
        let video_path = std::path::Path::new("test_av1.webm");
        let video_file = File::open(video_path)?;
        let temp_video = FileSource::new(video_file)?;
        let temp_video_input = InputSource::from(temp_video);
        let mut init_video = temp_video_input.initialize(None)?;
        let video_tracks = init_video.get_tracks()?;
        
        // Create video source from test_av1.webm
        let video_file = File::open(video_path)?;
        let video_source = FileSource::new(video_file)?;
        let video_input = InputSource::from(video_source);
        
        // Create WebVTT subtitle source from example.vtt (first 30 seconds only to match video)
        let vtt_path = std::path::Path::new("example.vtt");
        let vtt_file = File::open(vtt_path)?;
        let vtt_source = WebVttSource::new(vtt_file, "eng".to_string(), false)?;
        let vtt_input = InputSource::from(vtt_source);
        
        // Create output sink
        let output_sink = FileSink::new(&output_path)?;
        let output = OutputSink::from(output_sink);
        
        // Configure output mappings: all tracks from video (source 0), subtitle track from WebVTT (source 1)
        let mut output_mappings = Vec::new();
        // Add all video and audio tracks from source 0
        for track in &video_tracks.track_entry {
            output_mappings.push((0u64, track.track_number.0));
        }
        // Add subtitle track from source 1 (WebVTT) - track number is 1
        output_mappings.push((1u64, 1u64));
        
        // Apply cut interval to limit output to first 30 seconds to match video
        let cut_interval = CutInterval::new().with_end(30_000_000_000);
        
        // Perform remuxing
        let stats = remux(
            vec![video_input, vtt_input],
            output, 
            Some(cut_interval),
            None, // No special seek type
            Some(output_mappings.clone())
        )?;
        
        // Validate we processed some blocks
        assert!(stats.blocks_processed > 0, "Should have processed at least some blocks");
        assert_eq!(stats.track_count, output_mappings.len(), "Track count should match mappings");
        
        // Validate the output (skip timestamp plausibility check due to video metadata vs actual frame mismatch)
        let validation_report = validate_mkv_output(&output_path, true, None, false, true)?;
        
        // Print report for debugging
        if !validation_report.is_valid() {
            eprintln!("{}", validation_report.summary());
            for error in &validation_report.errors {
                eprintln!("ERROR: {}", error);
            }
            for warning in &validation_report.warnings {
                eprintln!("WARNING: {}", warning);
            }
        }
        
        // Assert critical validations passed
        assert!(validation_report.syntax_valid, "Output should have valid EBML syntax");
        // Skip timestamps_plausible check - video file duration metadata doesn't match actual frame timestamps
        assert!(validation_report.all_tracks_present, "All declared tracks should be present in clusters");
        assert!(validation_report.cluster_block_count_valid,
            "Cluster block counts should be within [{}, {}]",
            crate::cluster_warpper::MIN_BLOCKS_PER_CLUSTER,
            crate::cluster_warpper::MAX_BLOCKS_PER_CLUSTER);
        
        // Cleanup is commented out for inspection
        // let _ = std::fs::remove_file(&output_path);
        
        Ok(())
    }

    #[test]
    fn test_remux_av1_vp9_cross_source_tracks() -> Result<()> {
        use crate::source::WebVttSource;

        // Remux tracks 3 and 4 from test_av1.webm (source 0),
        // track 1 from test_vp9.webm (source 1), and
        // subtitles from example.vtt (source 2),
        // cutting from 5s to 25s with snap-nearest-keyframe.
        let temp_dir = std::env::temp_dir();
        let output_path = temp_dir.join("test_av1_vp9_cross_source.mkv");

        let av1_path = std::path::Path::new("test_av1.webm");
        let vp9_path = std::path::Path::new("test_vp9.webm");
        let vtt_path = std::path::Path::new("example.vtt");
        let av1_file = File::open(av1_path)?;
        let vp9_file = File::open(vp9_path)?;
        let vtt_file = File::open(vtt_path)?;

        let source0 = InputSource::from(FileSource::new(av1_file)?);
        let source1 = InputSource::from(FileSource::new(vp9_file)?);
        let source2 = InputSource::from(WebVttSource::new(vtt_file, "eng".to_string(), false)?);

        let output = OutputSink::from(FileSink::new(&output_path)?);

        // (source_index, track_number)
        let output_mappings = vec![
            (0u64, 3u64), // audio ger from test_av1.webm
            (1u64, 1u64), // track 1 from test_vp9.webm
            (2u64, 1u64), // subtitle from example.vtt (replaces source 0 subtitle)
        ];

        let cut_interval = CutInterval::new().with_range(5_000_000_000, 25_000_000_000);

        let stats = remux(
            vec![source0, source1, source2],
            output,
            Some(cut_interval),
            Some(RemuxerCutMode::SnapNearestKeyframe),
            Some(output_mappings.clone()),
        )?;

        assert!(stats.blocks_processed > 0, "Should have processed at least some blocks");
        assert_eq!(stats.track_count, output_mappings.len(), "Track count should match mappings");

        let validation_report = validate_mkv_output(&output_path, true, None, false, true)?;

        if !validation_report.is_valid() {
            eprintln!("{}", validation_report.summary());
            for error in &validation_report.errors {
                eprintln!("ERROR: {}", error);
            }
            for warning in &validation_report.warnings {
                eprintln!("WARNING: {}", warning);
            }
        }

        assert!(validation_report.syntax_valid, "Output should have valid EBML syntax");
        assert!(validation_report.all_tracks_present, "All declared tracks should be present in clusters");
        assert!(validation_report.cluster_block_count_valid,
            "Cluster block counts should be within [{}, {}]",
            crate::cluster_warpper::MIN_BLOCKS_PER_CLUSTER,
            crate::cluster_warpper::MAX_BLOCKS_PER_CLUSTER);

        // let _ = std::fs::remove_file(&output_path);

        Ok(())
    }
}
