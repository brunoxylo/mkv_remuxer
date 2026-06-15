use std::io::Write;
use std::sync::{Arc, Mutex};

use super::Sink;
use super::util::WebmFilterWriterSend;
use crate::{ContainerFormat, Error, Result};
use bytes::{BufMut, Bytes, BytesMut};
use log::trace;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;

/// Handle returned by [`ChunkedStreamSink::new`] that allows draining the
/// written data from outside the `Sink` trait.
#[derive(Clone)]
pub struct ChunkedSinkHandle {
    buffer: Arc<Mutex<BytesMut>>,
}

impl ChunkedSinkHandle {
    /// Take all buffered data as an immutable `Bytes`, leaving the buffer empty
    /// but keeping its allocation for reuse.
    pub fn next_segment(&self) -> Bytes {
        // unwrap allowed the bc when the mutex panicks other parts of the app panicked before
        let mut buf = self.buffer.lock().unwrap();
        buf.split().freeze()
    }

    /// Returns the number of bytes currently buffered.
    pub fn len(&self) -> usize {
        let buf = self.buffer.lock().unwrap();
        buf.len()
    }

    pub fn is_empty(&self) -> bool {
        let buf = self.buffer.lock().unwrap();
        buf.is_empty()
    }
}

/// Stream-based sink that writes MKV/WebM data into a shared in-memory buffer.
///
/// Create via [`ChunkedStreamSink::new`] which returns both the sink (to pass
/// into a `Remuxer`) and a [`ChunkedSinkHandle`] (to drain buffered chunks
/// after each processing step).
pub struct ChunkedStreamSink {
    buffer: Arc<Mutex<BytesMut>>,
    container_format: ContainerFormat,
    timescale: u64,
}

impl ChunkedStreamSink {
    /// Create a new chunked stream sink.
    ///
    /// Returns `(sink, handle)` — pass the sink into a `Remuxer` via
    /// `OutputSink::from(sink)`, and use the handle to drain chunks.
    pub fn new() -> (Self, ChunkedSinkHandle) {
        let buffer = Arc::new(Mutex::new(BytesMut::new()));
        let handle = ChunkedSinkHandle {
            buffer: Arc::clone(&buffer),
        };
        let sink = Self {
            buffer,
            container_format: ContainerFormat::Mkv,
            timescale: 1_000_000,
        };
        (sink, handle)
    }
}

impl Sink for ChunkedStreamSink {
    fn initialize(
        &mut self,
        tracks: &Tracks,
        info: &Info,
        ebml_header: &Ebml,
        chapters: Option<&Chapters>,
    ) -> Result<()> {
        self.container_format = match ebml_header.doc_type {
            Some(DocType(ref doc_type))
                if doc_type.to_lowercase() == ContainerFormat::Mkv.to_string() =>
            {
                ContainerFormat::Mkv
            }
            Some(DocType(ref doc_type))
                if doc_type.to_lowercase() == ContainerFormat::WebM.to_string() =>
            {
                ContainerFormat::WebM
            }
            _ => {
                return Err(Error::InvalidConfig(format!(
                    "EBML header doc type must be mkv or webm for StreamSink",
                )));
            }
        };
        let is_webm = self.container_format == ContainerFormat::WebM;
        let mut buf = self.buffer.lock().unwrap();
        let mut writer = WebmFilterWriterSend::new((&mut *buf).writer(), is_webm);
        // Write EBML header
        ebml_header.write_to(&mut writer)?;

        // Write Segment start with unknown size for streaming
        // Segment ID is 0x18538067
        writer.write_all(&[0x18, 0x53, 0x80, 0x67])?;

        // Unknown size marker (all 1s in VINT encoding)
        writer.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;

        // Write Info element inside the segment
        info.write_to(&mut writer)?;

        // Write Tracks element inside the segment
        tracks.write_to(&mut writer)?;

        // Write Chapters element inside the segment (if present)
        if let Some(chapters) = chapters {
            chapters.write_to(&mut writer)?;
        }
        writer.flush()?;

        // Store timescale for timestamp calculations
        self.timescale = info.timestamp_scale.0;
        Ok(())
    }

    fn write_cluster(&mut self, cluster: &Cluster, _track_number: u64) -> Result<()> {
        // Calculate cluster timestamp in nanoseconds
        let cluster_timestamp_ticks = cluster.timestamp.0;
        let cluster_timestamp_ns = cluster_timestamp_ticks * self.timescale;
        let is_webm = self.container_format == ContainerFormat::WebM;
        let mut buf = self.buffer.lock().unwrap();
        let mut writer = WebmFilterWriterSend::new((&mut *buf).writer(), is_webm);
        cluster.write_to(&mut writer)?;
        trace!("written cluster at timestamp {} ns", cluster_timestamp_ns);
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        let mut buf = self.buffer.lock().unwrap();
        *buf = BytesMut::new();
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
