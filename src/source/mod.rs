use crate::Result;
use mkv_element::prelude::*;
use std::fmt::Display;
use std::marker::PhantomData;

mod cluster_cache;
mod file_source;

pub use cluster_cache::ClusterOfInterestCache;
pub use file_source::FileSource;

// Typestate marker types
/// Marker type indicating the source has not been initialized
pub struct Uninitialized;

/// Marker type indicating the source has been initialized
pub struct Initialized;

#[derive(Debug, Clone, PartialEq)]
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
pub trait Source: Display {
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

    fn get_cut_positions(&self) -> (u64, Option<u64>);
    fn get_duration(& mut self) -> Result<u64>;

    // function to get target timecode scale for output (nanoseconds per time unit)
    fn get_target_timecode_scale(&self) -> Result<u64>;
    fn initialize(&mut self, output_time_scale: Option<u64>) -> Result<()>;
    /// set start and end position in ns for the source (for seeking)
    /// Returns the offset to the reference keyframe from the specified start position in ns
    fn initialize_with_cut(
        &mut self,
        output_time_scale: Option<u64>,
        seek_type: SeekType,
        start_ns: Option<u64>,
        end_ns: Option<u64>,
    ) -> Result<i64>;
}

/// Wrapper struct that uses the typestate pattern to prevent misuse
pub struct InputSource<State = Uninitialized> {
    inner: Box<dyn Source>,
    _state: PhantomData<State>,
}

impl<State> Display for InputSource<State> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
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
        sources
            .into_iter()
            .map(|source| Self::new(Box::new(source)))
            .collect()
    }

    /// Create multiple uninitialized input sources from a vec of boxed trait objects
    pub fn from_boxed_vec(sources: Vec<Box<dyn Source>>) -> Vec<Self> {
        sources.into_iter().map(Self::new).collect()
    }

    /// Create multiple uninitialized input sources from an array of concrete Source implementations
    pub fn from_array<T: Source + 'static, const N: usize>(sources: [T; N]) -> Vec<Self> {
        sources
            .into_iter()
            .map(|source| Self::new(Box::new(source)))
            .collect()
    }

    /// Initialize the source with optional custom time scale
    pub fn initialize(
        mut self,
        output_time_scale: Option<u64>,
    ) -> Result<InputSource<Initialized>> {
        // Delegate to the inner Source implementation
        self.inner.initialize(output_time_scale)?;

        // Transition to initialized state
        Ok(InputSource {
            inner: self.inner,
            _state: PhantomData,
        })
    }

    /// Initialize the source with cutting parameters
    /// returns the offset to the reference keyframe from the specified start position in ns
    pub fn initialize_with_cut(
        mut self,
        time_scale: Option<u64>,
        seek_type: SeekType,
        start_ns: Option<u64>,
        end_ns: Option<u64>,
    ) -> Result<(InputSource<Initialized>, i64)> {
        // Delegate to the inner Source implementation
        let offsets = self
            .inner
            .initialize_with_cut(time_scale, seek_type, start_ns, end_ns)?;

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

    pub fn get_cut_positions(&self) -> (u64, Option<u64>) {
        self.inner.get_cut_positions()
    }

    pub fn get_duration(&mut self) -> Result<u64> {
        self.inner.get_duration()
    }
}

#[cfg(test)]
use crate::test_utils;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::block_ext::{ClusterBlockExt, TrackKind, TracksExt};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    const ONE_SEC_NS: u64 = 1_000_000_000;
    const CUT_START_NS: u64 = 5_000_000_000;
    const CUT_END_NS: u64 = 15_000_000_000;
    const CUT_MAX_NS: u64 = CUT_END_NS - CUT_START_NS;

    const SEEK_TYPES: [SeekType; 4] = [
        SeekType::Freeze,
        SeekType::Squeeze,
        SeekType::SnapNearestKeyframe,
        SeekType::DirtyCut,
    ];

    fn test_file_path() -> PathBuf {
        test_utils::test_file_path()
    }

    fn sources_implementations() -> Vec<InputSource<Uninitialized>> {
        test_utils::sources_implementations()
    }

    fn validate_stream(mut source: InputSource<Initialized>, max_ts_ns: Option<u64>) -> Result<()> {
        let tracks = source.get_tracks()?;
        let timecode_scale = source.get_target_timecode_scale()?;
        let mut last_by_track: HashMap<u64, i64> = HashMap::new();
        let mut first_by_track: HashMap<u64, i64> = HashMap::new();
        let mut saw_audio_video = false;

        loop {
            let cluster = match source.get_next_cluster()? {
                Some(cluster) => cluster,
                None => break,
            };

            let cluster_ticks = cluster.timestamp.0 as i64;
            for block in cluster.blocks {
                let track_num = block.track_number()?;
                let kind = tracks.get_track_kind(track_num);
                let is_av = matches!(kind, Some(TrackKind::Audio | TrackKind::Video));
                if !is_av {
                    continue;
                }
                saw_audio_video = true;

                let ts = block.timestamp_ns(cluster_ticks, timecode_scale)?;
                assert!(ts >= 0, "track {} has negative timestamp {}", track_num, ts);
                if let Some(prev) = last_by_track.get(&track_num) {
                    assert!(
                        ts >= *prev,
                        "track {} timestamp regressed: {} -> {}",
                        track_num,
                        prev,
                        ts
                    );
                }
                last_by_track.insert(track_num, ts);
                first_by_track.entry(track_num).or_insert(ts);

                if let Some(max_ts) = max_ts_ns {
                    assert!(
                        ts as u64 <= max_ts + 1500_000_000, // allow 150ms tolerance for cut accuracy (accounts for inter-track timing variations)
                        "track {} timestamp {} exceeds {}",
                        track_num,
                        ts,
                        max_ts
                    );
                }
            }
        }

        assert!(saw_audio_video, "no audio/video blocks found");
        for (track_num, first_ts) in first_by_track {
            assert!(
                first_ts >= 0 && (first_ts as u64) <= ONE_SEC_NS,
                "track {} starts at {}ns, expected 0..=1s",
                track_num,
                first_ts
            );
        }

        Ok(())
    }

    #[test]
    fn test_source_monotonic_and_start_times() -> Result<()> {
        assert!(test_file_path().exists(), "missing test.webm in repo root");

        for source in sources_implementations() {
            let source = source.initialize(None)?;
            let source_str = source.to_string();
            validate_stream(source, None)
                .map_err(|err| Error::InvalidConfig(format!("{}:{}", source_str, err)))?;
        }

        Ok(())
    }

    #[test]
    fn test_source_cut_5s_to_15s() -> Result<()> {
        assert!(test_file_path().exists(), "missing test.webm in repo root");

        for seek_type in SEEK_TYPES {
            println!("Testing seek type: {:?}", seek_type);
            for mut source in sources_implementations() {
                print!("  Source Implementation: {}... ", source);
                let (source, offset) = source.initialize_with_cut(
                    None,
                    seek_type.clone(),
                    Some(CUT_START_NS),
                    Some(CUT_END_NS),
                )?;
                let mut max_ts = CUT_MAX_NS;
                println!("our offset is {}", offset);
                if matches!(seek_type, SeekType::SnapNearestKeyframe) {
                    max_ts = (max_ts as i64 - offset as i64) as u64;
                }
                validate_stream(source, Some(max_ts))?;
            }
        }

        Ok(())
    }
}

/// Configuration for cutting/seeking behavior
#[derive(Debug, Clone)]
pub struct CutConfig {
    pub seek_type: SeekType,
    pub start_ns: Option<u64>,
    pub end_ns: Option<u64>,
}

impl CutConfig {
    pub fn new(seek_type: SeekType) -> Self {
        Self {
            seek_type,
            start_ns: None,
            end_ns: None,
        }
    }

    pub fn with_start(mut self, start_ns: u64) -> Self {
        self.start_ns = Some(start_ns);
        self
    }

    pub fn with_end(mut self, end_ns: u64) -> Self {
        self.end_ns = Some(end_ns);
        self
    }

    pub fn with_range(mut self, start_ns: u64, end_ns: u64) -> Self {
        self.start_ns = Some(start_ns);
        self.end_ns = Some(end_ns);
        self
    }
}