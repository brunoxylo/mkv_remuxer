use crate::Result;
use mkv_element::prelude::*;
use std::fmt::Display;
use std::marker::PhantomData;
use super::super::{Source, SeekType, CutInterval};

// Typestate marker types
/// Marker type indicating the source has not been initialized
pub struct Uninitialized;

/// Marker type indicating the source has been initialized and is ready for optional cutting.
pub struct Cutting;

/// Marker type indicating the source has been fully prepared and is streaming clusters.
pub struct Remuxing;

/// Wrapper struct that uses the typestate pattern to prevent misuse of Source trait implementations.
pub struct InputSource<State = Uninitialized> {
    inner: Box<dyn Source>,
    /// Populated after `initialize()` and updated by every `cut()` call.
    _state: PhantomData<State>,
}

impl<State> Display for InputSource<State> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
}

// ── Uninitialized ─────────────────────────────────────────────────────────────

impl InputSource<Uninitialized> {
    /// Create a new uninitialized input source wrapping any [`Source`] implementation.
    pub fn new(source: Box<dyn Source>) -> Self {
        Self { inner: source, _state: PhantomData }
    }

    /// Create multiple uninitialized sources from a `Vec` of concrete implementations.
    pub fn from_vec<T: Source + 'static>(sources: Vec<T>) -> Vec<Self> {
        sources.into_iter().map(|s| Self::new(Box::new(s))).collect()
    }

    /// Create multiple uninitialized sources from a `Vec` of boxed trait objects.
    pub fn from_boxed_vec(sources: Vec<Box<dyn Source>>) -> Vec<Self> {
        sources.into_iter().map(Self::new).collect()
    }

    /// Create multiple uninitialized sources from an array of concrete implementations.
    pub fn from_array<T: Source + 'static, const N: usize>(sources: [T; N]) -> Vec<Self> {
        sources.into_iter().map(|s| Self::new(Box::new(s))).collect()
    }

    /// **Uninitialized → Cutting.**
    ///
    /// Pipes to [`Source::initialize`].  Metadata methods become valid on the
    /// returned source; no cut has been applied yet.
    pub fn initialize(mut self, output_time_scale: Option<u64>) -> Result<InputSource<Cutting>> {
        let _cut_interval = self.inner.initialize(output_time_scale)?;
        Ok(InputSource { inner: self.inner, _state: PhantomData })
    }

    /// **Uninitialized → Remuxing** (skips the `Cutting` state entirely).
    ///
    /// Pipes to [`Source::initialize_into_remuxing`].  Use this when you do not
    /// need to inspect metadata before deciding on a cut.
    pub fn initialize_into_remuxing(mut self) -> Result<InputSource<Remuxing>> {
        let _cut_interval = self.inner.initialize_into_remuxing(None)?;
        Ok(InputSource { inner: self.inner, _state: PhantomData })
    }
}

// ── Shared metadata (Cutting + Remuxing) ─────────────────────────────────────

/// Expands to metadata accessor methods for both `Cutting` and `Remuxing`.
/// All methods are direct pipes to the inner [`Source`] implementation.
macro_rules! impl_metadata_methods {
    ($state:ty) => {
        impl InputSource<$state> {
            pub fn get_tracks(&self) -> Result<Tracks> { self.inner.get_tracks() }
            pub fn get_chapters(&self) -> Result<Option<Chapters>> { self.inner.get_chapters() }
            pub fn get_info(&self) -> Result<Info> { self.inner.get_info() }
            pub fn get_own_timecode_scale(&self) -> Result<u64> { self.inner.get_own_timecode_scale() }
            pub fn get_target_timecode_scale(&self) -> Result<u64> { self.inner.get_target_timecode_scale() }
            pub fn get_output_interval(&mut self) -> Result<CutInterval> { self.inner.get_output_interval() }
            pub fn get_output_duration(&mut self) -> Result<Option<u64>> { self.inner.get_output_duration() }
        }
    };
}

impl_metadata_methods!(Cutting);
impl_metadata_methods!(Remuxing);

// ── Cutting ───────────────────────────────────────────────────────────────────

impl InputSource<Cutting> {
    /// **Cutting (idempotent): apply or re-apply a cut interval.**
    ///
    /// Pipes to [`Source::cut`].  Returns the source with the updated interval
    /// and the *actual* interval after any keyframe snapping.
    pub fn cut(
        &mut self,
        seek_type: SeekType,
        interval: CutInterval,
    ) -> Result<CutInterval> {
        self.inner.cut(seek_type, interval)
    }

    /// **Cutting → Remuxing.**
    ///
    /// Pipes to [`Source::start_remuxing`].  The cut applied by the last `cut()`
    /// call (or the full-file default from `initialize()`) takes effect.
    pub fn into_remuxing(mut self) -> Result<InputSource<Remuxing>> {
        self.inner.start_remuxing()?;
        Ok(InputSource { inner: self.inner, _state: PhantomData })
    }

    /// **Convenience: cut then immediately transition to Remuxing.**
    ///
    /// Equivalent to `cut(output_timescale, seek_type, interval)` followed by
    /// `into_remuxing(None)` — pipes to [`Source::cut`] then [`Source::start_remuxing`].
    pub fn cut_into_remuxing(
        mut self,
        _output_timescale: Option<u64>,
        seek_type: SeekType,
        interval: CutInterval,
    ) -> Result<(InputSource<Remuxing>, CutInterval)> {
        let actual = self.inner.cut(seek_type, interval)?;
        self.inner.start_remuxing()?;
        Ok((
            InputSource { inner: self.inner, _state: PhantomData },
            actual,
        ))
    }
}

// ── Remuxing ──────────────────────────────────────────────────────────────────

impl InputSource<Remuxing> {
    /// Returns the next cluster, or `None` at end-of-stream.
    /// Pipes to [`Source::get_next_cluster`].
    pub fn get_next_cluster(&mut self) -> Result<Option<Cluster>> {
        self.inner.get_next_cluster()
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
