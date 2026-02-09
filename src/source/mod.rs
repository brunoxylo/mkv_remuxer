use crate::Result;
use mkv_element::prelude::*;
use std::marker::PhantomData;

mod file_source;
pub use file_source::FileSource;

// Typestate marker types
/// Marker type indicating the source has not been initialized
pub struct Uninitialized;

/// Marker type indicating the source has been initialized
pub struct Initialized;

#[derive(Debug, Clone)]
pub enum SeekType {
    /// (fast, not exact, nice) Seek to the nearest keyframe before or after the target timestamp
    SnapNearestKeyframe,
    /// (slow on client, exact, nice) Squeeze the frames from the previous keyframe up to the desired cut position to timestamp 0
    Squeeze, 
     /// (fast, exact, ugly) Move the next keyframe to timestamp 0 (while omitting frames in between), freezing the video from start until the original next keyframes position
    Freeze,
    // (fast, exact, ugly) Just cut at the exact timestamp, without respecting keyframe boundaries (may cause playback issues)
    DirtyCut,
}
/// Represents a source of MKV data (input file or stream)
pub trait Source {
    /// Get the track information from the source
    /// Returns the Tracks element containing all audio/video/subtitle tracks
    fn get_tracks(&self) -> Result<Tracks>;
    
    /// Get chapter information from the source
    /// Returns None if the source has no chapters
    fn get_chapters(&self) -> Result<Option<Chapters>>;
    
    /// Get segment metadata/info from the source
    /// Returns the Info element with duration, title, timestamps, etc.
    fn get_info(&self) -> Result<Info>;
    
    /// Get the next block/frame of data from the source
    /// Returns None when end of stream is reached
    fn get_next_cluster(&mut self) -> Result<Option<Cluster>>;

    /// fuction to get the sources timescale (nanoseconds per time unit)
    fn get_own_timecode_scale(&self) -> Result<u64>;

    // function to get target timecode scale for output (nanoseconds per time unit)
    fn get_target_timecode_scale(&self) -> Result<u64>;
    fn initialize(&mut self, output_time_scale: Option<u64>) -> Result<()>;
    /// set start and end position in ns for the source (for seeking)
    /// Returns the offset to the reference keyframe for start and end position
    fn initialize_with_cut(&mut self, output_time_scale: Option<u64>, seek_type: SeekType, start_ns: Option<u64>, end_ns: Option<u64>) -> Result<(u64, u64)>;
    
}

/// Wrapper struct that uses the typestate pattern to prevent misuse
pub struct InputSource<State = Uninitialized> {
    inner: Box<dyn Source>,
    _state: PhantomData<State>,
}

// Implementation for uninitialized source
impl InputSource<Uninitialized> {
    /// Create a new uninitialized input source wrapping any Source implementation
    pub fn new(source: Box<dyn Source>) -> Self {
        Self {
            inner: source,
            _state: PhantomData,
        }
    }
    
    /// Create multiple uninitialized input sources from a vec of concrete Source implementations
    pub fn from_vec<T: Source + 'static>(sources: Vec<T>) -> Vec<Self> {
        sources.into_iter()
            .map(|source| Self::new(Box::new(source)))
            .collect()
    }
    
    /// Create multiple uninitialized input sources from a vec of boxed trait objects
    pub fn from_boxed_vec(sources: Vec<Box<dyn Source>>) -> Vec<Self> {
        sources.into_iter()
            .map(Self::new)
            .collect()
    }
    
    /// Create multiple uninitialized input sources from an array of concrete Source implementations
    pub fn from_array<T: Source + 'static, const N: usize>(sources: [T; N]) -> Vec<Self> {
        sources.into_iter()
            .map(|source| Self::new(Box::new(source)))
            .collect()
    }

    /// Initialize the source with optional custom time scale
    pub fn initialize(mut self, output_time_scale: Option<u64>) -> Result<InputSource<Initialized>> {
        // Delegate to the inner Source implementation
        self.inner.initialize(output_time_scale)?;
        
        // Transition to initialized state
        Ok(InputSource {
            inner: self.inner,
            _state: PhantomData,
        })
    }

    /// Initialize the source with cutting parameters
    pub fn initialize_with_cut(
        mut self,
        time_scale: Option<u64>,
        seek_type: SeekType,
        start_ns: Option<u64>,
        end_ns: Option<u64>,
    ) -> Result<(InputSource<Initialized>, (u64, u64))> {
        // Delegate to the inner Source implementation
        let offsets = self.inner.initialize_with_cut(time_scale, seek_type, start_ns, end_ns)?;
        
        // Transition to initialized state
        Ok((
            InputSource {
                inner: self.inner,
                _state: PhantomData,
            },
            offsets,
        ))
    }
}

// From trait implementations for automatic conversion
impl From<Box<dyn Source>> for InputSource<Uninitialized> {
    fn from(source: Box<dyn Source>) -> Self {
        Self::new(source)
    }
}

impl<T: Source + 'static> From<T> for InputSource<Uninitialized> {
    fn from(source: T) -> Self {
        Self::new(Box::new(source))
    }
}

// Implementation for initialized source
impl InputSource<Initialized> {
    /// Get the track information from the source
    pub fn get_tracks(&self) -> Result<Tracks> {
        self.inner.get_tracks()
    }
    
    /// Get chapter information from the source
    pub fn get_chapters(&self) -> Result<Option<Chapters>> {
        self.inner.get_chapters()
    }
    
    /// Get segment metadata/info from the source
    pub fn get_info(&self) -> Result<Info> {
        self.inner.get_info()
    }
    
    /// Get the next block/frame of data from the source
    pub fn get_next_cluster(&mut self) -> Result<Option<Cluster>> {
        self.inner.get_next_cluster()
    }
    
    /// Get the source's timecode scale (nanoseconds per time unit)
    pub fn get_own_timecode_scale(&self) -> Result<u64> {
        self.inner.get_own_timecode_scale()
    }
    
    /// Get the target timecode scale for output (nanoseconds per time unit)
    pub fn get_target_timecode_scale(&self) -> Result<u64> {
        self.inner.get_target_timecode_scale()
    }
}
