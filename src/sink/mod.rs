use crate::Result;
use crate::APP_NAME;
use mkv_element::prelude::*;
use std::marker::PhantomData;

mod file_sink;
pub use file_sink::FileSink;

// Typestate marker types
pub struct Uninitialized;
pub struct Initialized;

/// Represents a sink/destination for MKV data (output file or stream)
pub trait Sink {
    /// Initialize the output with EBML header and segment info
    fn initialize(&mut self, tracks: &Tracks, info: &Info, chapters: Option<&Chapters>) -> Result<()>;
    
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
        chapters: Option<&Chapters>
    ) -> Result<OutputSink<Initialized>> {
        // Delegate to the inner Sink implementation
        self.inner.initialize(tracks, info, chapters)?;
        
        // Transition to initialized state
        Ok(OutputSink {
            inner: self.inner,
            _state: PhantomData,
        })
    }

    pub fn initialize_simple(self, tracks: &Tracks, duration_ns: u64, timecode_scale: u64) -> Result<OutputSink<Initialized>> {
        let info = Info {
            timestamp_scale: TimestampScale(timecode_scale),
            muxing_app: MuxingApp(APP_NAME.to_string()),
            writing_app: WritingApp(APP_NAME.to_string()),
            duration: Some(Duration(duration_ns as f64)),
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