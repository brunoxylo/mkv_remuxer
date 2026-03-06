use crate::{Result, source::util::basic_info::MkvBasicInfo};
use mkv_element::prelude::*;
use std::fmt::Display;

pub mod util;
mod file_source;
mod webvtt_source;

pub use util::{KeyframePositionCache, InputSource, Uninitialized, Cutting, Remuxing, MkvReader};
pub use file_source::FileSource;
pub use webvtt_source::WebVttSource;

/// Represents a source of MKV data (input file or stream).
///
/// # Phase contract
///
/// Implementations must support the following call sequences — only these
/// orderings are valid:
/// The Input SourceWarpper uses the typestate pattern to enforce these call sequences at compile time, preventing misuse. The valid sequences are:
///
/// **With optional cutting:**
/// ```text
/// initialize()
/// cut()*            ← idempotent; may be called any number of times
/// start_remuxing()
/// get_next_cluster()*
/// ```
///
/// **Skipping the Cutting phase entirely:**
/// ```text
/// initialize_into_remuxing()   ← calls initialize() + start_remuxing() internally
/// get_next_cluster()*
/// ```
///
/// Calling `cut()` before `initialize()` or after `start_remuxing()` is
/// undefined behaviour.  Calling `get_next_cluster()` before `start_remuxing()`
/// (or `initialize_into_remuxing()`) is undefined behaviour.
pub trait Source: Display + Send {
    // ── Metadata ──────────────────────────────────────────────────────────────
    // All metadata methods are valid after `initialize()`.

    /// Track list for this source.
    fn get_tracks(&self) -> Result<Tracks>;
    /// Chapter list, or `None` if the source has no chapters.
    fn get_chapters(&self) -> Result<Option<Chapters>>;
    /// Segment info element (duration, title, timestamps, …).
    fn get_info(&self) -> Result<Info>;
    fn get_basic_info(&self)  -> Result<MkvBasicInfo>;
    /// The source's own timecode scale (nanoseconds per tick).
    fn get_own_timecode_scale(&self) -> Result<u64>;
    /// The target output timecode scale (nanoseconds per tick).
    fn get_target_timecode_scale(&self) -> Result<u64>;
    /// The (start_ns, end_ns) cut nanoseconds positions currently applied.
    /// should return (0, OriginalDuration) when no cut applied, and update accordingly after each cut.
    fn get_output_interval(&mut self) -> Result<CutInterval>;
    /// Effective duration in nanoseconds, respecting any applied cut.
    fn get_output_duration(&mut self) -> Result<Option<u64>> {
        let start = self.get_output_interval()?.start_ns.unwrap_or(0);
        if let Some(end) = self.get_output_interval()?.end_ns {
            if end >= start {
                return Ok(Some(end - start));
            }
        }
        Ok(None)
    }

    // ── Phase transitions ─────────────────────────────────────────────────────

    /// **Uninitialized → Cutting.**
    ///
    /// Reads headers and populates metadata.  Seeks to the first cluster.
    /// `output_time_scale` sets the output timescale; uses the source's own
    /// scale when `None`.  Returns the full-file interval (no cut applied yet).
    fn initialize(&mut self, output_time_scale: Option<u64>) -> Result<CutInterval>;

    /// **Cutting (idempotent): apply or re-apply a cut interval.**
    ///
    /// Each call replaces the previous cut.  The returned interval reflects any
    /// keyframe snapping that occurred (may differ from `interval` for
    /// `SnapNearestKeyframe` / `SnapPreviousKeyframe`).
    ///
    /// MUST NOT be called before `initialize()` or after `start_remuxing()`.
    fn cut(
        &mut self,
        seek_type: SeekType,
        interval: CutInterval,
    ) -> Result<CutInterval>;

    /// **Cutting → Remuxing.**
    ///
    /// Seals the current cut (or the full-file interval from `initialize()`)
    /// and seeks to the start cluster.  After this call `cut()` MUST NOT be
    /// called; `get_next_cluster()` becomes valid.
    ///
    /// `output_time_scale` may override the scale set during `initialize()` /
    /// `cut()`.
    fn start_remuxing(&mut self) -> Result<()>;

    /// **Uninitialized → Remuxing in one step** (skips the `Cutting` state).
    ///
    /// The default implementation calls `initialize(output_time_scale)` followed
    /// by `start_remuxing()`.  Implementors may override for efficiency.
    fn initialize_into_remuxing(
        &mut self,
        output_time_scale: Option<u64>,
    ) -> Result<CutInterval> {
        let interval = self.initialize(output_time_scale)?;
        self.start_remuxing()?;
        Ok(interval)
    }

    // ── Remuxing ──────────────────────────────────────────────────────────────
    // Only valid after `start_remuxing()` (or `initialize_into_remuxing()`).

    /// Returns the next cluster, or `None` at end-of-stream.
    fn get_next_cluster(&mut self) -> Result<Option<Cluster>>;
}


#[derive(Debug, Clone, PartialEq)]
pub enum SeekType {
    /// (fast, not exact, nice) Seek to the nearest keyframe before or after the target timestamp
    /// The integer holds the mkv track number of the track that is used as reference for searching keyframes
    SnapNearestKeyframe(u64),
    /// (fast, not exact, nice) Seek to the nearest keyframe before the target timestamp
    /// The integer holds the mkv track number of the track that is used as reference for searching keyframes
    SnapPreviousKeyframe(u64),
    /// (fast, not exact, nice) Seek to the nearest keyframe after the target timestamp
    /// The integer holds the mkv track number of the track that is used as reference for searching keyframes
    SnapNextKeyframe(u64),
    /// (slow on client, exact, nice) Squeeze the frames from the previous keyframe up to the desired cut position to timestamp 
    Squeeze,
    // (fast, exact, ugly) Just cut at the exact timestamp, without respecting keyframe boundaries (may cause playback issues)
    DirtyCut,
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

    const SEEK_TYPES: [SeekType; 5] = [
        SeekType::Squeeze,
        SeekType::SnapNearestKeyframe(1), // fot testing we assume that video track has number 1
        SeekType::SnapPreviousKeyframe(1),
        SeekType::SnapNextKeyframe(1),
        SeekType::DirtyCut,
    ];

    fn test_file_path() -> PathBuf {
        test_utils::test_file_path()
    }

    fn sources_implementations() -> Vec<InputSource<Uninitialized>> {
        test_utils::sources_implementations()
    }

    fn validate_stream(mut source: InputSource<Remuxing>, max_ts_ns: Option<u64>) -> Result<()> {
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
                        ts as u64 <= max_ts, // allow 10s tolerance for cut accuracy (accounts for inter-track timing variations)
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
            let source = source.initialize(None)?.into_remuxing()?;
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
            for source in sources_implementations() {
                print!("  Source Implementation: {}... ", source);
                let (source, offset) = source.initialize(None)?.cut_into_remuxing(
                    None,
                    seek_type.clone(),
                    CutInterval { start_ns: Some(CUT_START_NS), end_ns: Some(CUT_END_NS) },
                )?;
                let mut max_ts = CUT_MAX_NS;
                println!("our actual interval is {}", offset);
                if matches!(seek_type, SeekType::SnapNearestKeyframe(_) | SeekType::SnapPreviousKeyframe(_) | SeekType::SnapNextKeyframe(_)) {
                    // Calculate the actual duration from the keyframe timestamps
                    if let (Some(start), Some(end)) = (offset.start_ns, offset.end_ns) {
                        max_ts = end - start;
                        if matches!(seek_type, SeekType::SnapNearestKeyframe(_) | SeekType::SnapPreviousKeyframe(_) | SeekType::SnapNextKeyframe(_)) {
                            max_ts += 5_000_000_000; // allow 5s tolerance for snap nearest keyframe, we cnt get this universally for every input source to we just assume a key frame interval of 5s 
                        }
                    }
                }
                validate_stream(source, Some(max_ts))?;
            }
        }

        Ok(())
    }
}

/// Configuration for cutting/seeking behavior
#[derive(Debug, Clone, PartialEq, Copy)]
pub struct CutInterval {
    pub start_ns: Option<u64>,
    pub end_ns: Option<u64>,
}


impl CutInterval {
    pub fn new() -> Self {
        Self {
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

impl Display for CutInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const NS_PER_SEC: u64 = 1_000_000_000;
        match (self.start_ns, self.end_ns) {
            (Some(start), Some(end)) => write!(f, "{}s -> {}s", start / NS_PER_SEC, end / NS_PER_SEC),
            (Some(start), None) => write!(f, "{}s -> ∞", start / NS_PER_SEC),
            (None, Some(end)) => write!(f, "0s -> {}s", end / NS_PER_SEC),
            (None, None) => write!(f, "no cut"),
        }
    }
}