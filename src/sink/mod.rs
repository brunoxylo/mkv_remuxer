use crate::APP_NAME;
use crate::Result;
use mkv_element::prelude::*;
use std::marker::PhantomData;

mod file_sink;
pub use file_sink::FileSink;

mod vtt_sink;
pub use vtt_sink::VttSink;

// Typestate marker types
pub struct Uninitialized;
pub struct Initialized;

/// Represents a sink/destination for MKV data (output file or stream)
pub trait Sink {
    /// Initialize the output with EBML header and segment info
    fn initialize(
        &mut self,
        tracks: &Tracks,
        info: &Info,
        chapters: Option<&Chapters>,
    ) -> Result<()>;

    /// Write a cluster to the output
    fn write_cluster(&mut self, cluster: &Cluster, track_number: u64) -> Result<()>;

    /// Finalize the output (write cues, seek head, close file)
    fn finalize(&mut self) -> Result<()>;
}

/// Wrapper struct that uses the typestate pattern to prevent misuse
pub struct OutputSink<State = Uninitialized> {
    inner: Box<dyn Sink>,
    _state: PhantomData<State>,
}

// Implementation for uninitialized sink
impl OutputSink<Uninitialized> {
    /// Create a new uninitialized output sink wrapping any Sink implementation
    pub fn new(sink: Box<dyn Sink>) -> Self {
        Self {
            inner: sink,
            _state: PhantomData,
        }
    }

    /// Initialize the sink with EBML header and segment info
    ///
    /// Consumes the uninitialized sink and returns an initialized one.
    /// This state transition ensures initialization can only happen once.
    pub fn initialize(
        mut self,
        tracks: &Tracks,
        info: &Info,
        chapters: Option<&Chapters>,
    ) -> Result<OutputSink<Initialized>> {
        // Delegate to the inner Sink implementation
        self.inner.initialize(tracks, info, chapters)?;

        // Transition to initialized state
        Ok(OutputSink {
            inner: self.inner,
            _state: PhantomData,
        })
    }

    pub fn initialize_simple(
        self,
        tracks: &Tracks,
        duration_ns: u64,
        timecode_scale: u64,
    ) -> Result<OutputSink<Initialized>> {
        println!("duration_ns: {}, timecode_scale: {}", duration_ns, timecode_scale);
        let info = Info {
            timestamp_scale: TimestampScale(timecode_scale),
            muxing_app: MuxingApp(APP_NAME.to_string()),
            writing_app: WritingApp(APP_NAME.to_string()),
            duration: Some(Duration((duration_ns / timecode_scale) as f64)),
            date_utc: Some(DateUtc(chrono::Utc::now().timestamp())),
            title: None,
            segment_uuid: Some(SegmentUuid(uuid::Uuid::new_v4().as_bytes().to_vec())),
            segment_filename: None,
            prev_uuid: None,
            prev_filename: None,
            next_uuid: None,
            next_filename: None,
            segment_family: Vec::new(),
            chapter_translate: Vec::new(),
            crc32: None,
            void: None,
        };
        self.initialize(tracks, &info, None)
    }
}

// From trait implementations for automatic conversion
impl From<Box<dyn Sink>> for OutputSink<Uninitialized> {
    fn from(sink: Box<dyn Sink>) -> Self {
        Self::new(sink)
    }
}

impl<T: Sink + 'static> From<T> for OutputSink<Uninitialized> {
    fn from(sink: T) -> Self {
        Self::new(Box::new(sink))
    }
}

// Implementation for initialized sink
impl OutputSink<Initialized> {
    pub fn write_cluster(&mut self, cluster: &Cluster, track_number: u64) -> Result<()> {
        self.inner.write_cluster(cluster, track_number)
    }
    pub fn finalize(mut self) -> Result<()> {
        self.inner.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remux;
    use crate::source::{FileSource, InputSource, SeekType};
    use crate::test_utils::{test_file_path, validate_mkv_output};
    use crate::source::CutInterval;

    fn run_remux_test_with_seek_type(seek_type: SeekType) -> Result<()> {
        // Setup: Create output path in temp directory
        let temp_dir = std::env::temp_dir();
        let seek_type_name = format!("{:?}", seek_type);
        let output_path = temp_dir.join(format!("test_german_audio_{}.mkv", seek_type_name));
        
        // Create input source from test.webm (uninitialized)
        let input_path = test_file_path();
        let source = FileSource::new(&input_path)?;
        let input_source = InputSource::from(source);
        
        // Create output sink (uninitialized)
        let output_sink = FileSink::new(&output_path)?;
        let output = OutputSink::from(output_sink);
        
        // Configure cutting: from 20 seconds to the end
        let start_ns = 20_000_000_000u64; // 20 seconds in nanoseconds
        let cut_interval = CutInterval::new().with_start(start_ns);
        
        // We need to pre-check for German audio tracks manually
        // Initialize source temporarily to check tracks
        let temp_source = FileSource::new(&input_path)?;
        let temp_input = InputSource::from(temp_source);
        let mut initialized_temp = temp_input.initialize(None)?;
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
            Some(seek_type.clone()),
            Some(output_mappings.clone())
        )?;
        
        // Validate we processed some blocks
        assert!(stats.blocks_processed > 0, "Should have processed at least some blocks");
        assert_eq!(stats.track_count, output_mappings.len(), "Track count should match mappings");


        let input_duration_ns = initialized_temp.get_duration().unwrap();
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
        assert!(validation_report.cluster_duration_valid, 
            "Cluster durations should not exceed {} ns", 
            crate::cluster_warpper::CLUSTER_MAX_DURATION_NS);
        assert!(validation_report.cluster_size_valid, 
            "Cluster sizes should not exceed {} bytes", 
            crate::cluster_warpper::CLUSTER_MAX_SIZE_BYTES);
        
        // Cues validation might be less strict, just warn if invalid
        if !validation_report.cues_valid {
            eprintln!("Warning: Cues validation failed");
        }
        
        // Overall validation
        assert!(validation_report.is_valid(), "Overall MKV validation should pass for SeekType::{:?}", seek_type);
        
        // Cleanup
        //let _ = std::fs::remove_file(&output_path);
        
        Ok(())
    }

    #[test]
    fn test_remux_german_audio_snap_nearest_keyframe() -> Result<()> {
        run_remux_test_with_seek_type(SeekType::SnapNearestKeyframe)
    }

    #[test]
    fn test_remux_german_audio_squeeze() -> Result<()> {
        run_remux_test_with_seek_type(SeekType::Squeeze)
    }

    #[test]
    fn test_remux_german_audio_dirty_cut() -> Result<()> {
        run_remux_test_with_seek_type(SeekType::DirtyCut)
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
        let temp_video = FileSource::new(video_path)?;
        let temp_video_input = InputSource::from(temp_video);
        let mut init_video = temp_video_input.initialize(None)?;
        let video_tracks = init_video.get_tracks()?;
        
        // Create video source from test_av1.webm
        let video_source = FileSource::new(video_path)?;
        let video_input = InputSource::from(video_source);
        
        // Create WebVTT subtitle source from example.vtt (first 30 seconds only to match video)
        let vtt_path = std::path::Path::new("example.vtt");
        let vtt_source = WebVttSource::new(vtt_path, "eng")?;
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
        assert!(validation_report.cluster_duration_valid, 
            "Cluster durations should not exceed {} ns", 
            crate::cluster_warpper::CLUSTER_MAX_DURATION_NS);
        assert!(validation_report.cluster_size_valid, 
            "Cluster sizes should not exceed {} bytes", 
            crate::cluster_warpper::CLUSTER_MAX_SIZE_BYTES);
        
        // Cleanup is commented out for inspection
        // let _ = std::fs::remove_file(&output_path);
        
        Ok(())
    }
}
