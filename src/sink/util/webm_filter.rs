//! EBML-aware Write wrapper that filters out non-WebM elements.
//!
//! When writing to a WebM container, any EBML elements whose IDs are not in the
//! WebM whitelist (generated at build-time from `ebml_matroska.xml`) are replaced
//! with `Void` elements so that the binary layout stays intact but non-compliant
//! elements are silenced.
//!
//! Two wrapper types are provided:
//! - [`WebmFilterWriter`] for `W: Write` (used by `FileSink` with `BufWriter<File>`)
//! - [`WebmFilterWriterSend`] for `W: Write + Send` (used by `StreamSink`)
//!
//! This is necessary because the mkv_element library is not webm aware. And some mandatory mkv
//! elements such as audio emphasis are not supported in webm.

use log::trace;
use mkv_element::io::blocking_impl::ReadFrom;
use mkv_element::prelude::{Header, VInt64};
use std::io::{self, Cursor, Seek, SeekFrom, Write};

// Pull in the build-time generated whitelist
mod webm_whitelist {
    include!(concat!(env!("OUT_DIR"), "/webm_whitelist.rs"));
}
pub use webm_whitelist::{
    WEBM_ELEMENT_COUNT, WEBM_ELEMENT_IDS, is_master_element, is_webm_element,
};

// ─── Void encoding ─────────────────────────────────────────────────────────

/// Encode a Void EBML element that occupies exactly `total_bytes` on the wire.
/// Returns `None` if `total_bytes` is too small (< 2 bytes for the shortest Void).
fn encode_void_element(total_bytes: usize) -> Option<Vec<u8>> {
    if total_bytes < 2 {
        return None;
    }
    // Void element ID is 0xEC (1 byte).
    // We need: 1 (ID) + vint_len (size) + payload = total_bytes
    // Payload is all zeros.
    for vint_len in 1..=8usize {
        let payload = total_bytes.checked_sub(1 + vint_len)?;
        let max_payload = if vint_len == 8 {
            u64::MAX >> 1
        } else {
            (1u64 << (7 * vint_len)) - 2
        };
        if payload as u64 <= max_payload {
            let mut buf = Vec::with_capacity(total_bytes);
            buf.push(0xEC); // Void element ID
            let marker = 1u64 << (8 * vint_len - vint_len);
            let encoded = marker | (payload as u64);
            for i in (0..vint_len).rev() {
                buf.push((encoded >> (8 * i)) as u8);
            }
            buf.resize(total_bytes, 0x00);
            return Some(buf);
        }
    }
    None
}

// ─── Core filtering logic ──────────────────────────────────────────────────

/// Shared filter logic. Scans a buffer for EBML element headers and replaces
/// non-WebM elements with Void. No state is kept between write() calls —
/// each buffer is assumed to contain complete elements (as produced by
/// `mkv_element::write_to()`).
struct FilterCore<W> {
    inner: W,
    enabled: bool,
}

impl<W> FilterCore<W> {
    fn new(inner: W, enabled: bool) -> Self {
        Self { inner, enabled }
    }
}

impl<W: Write> FilterCore<W> {
    /// Scan `buf` for EBML elements. Legal elements are passed through;
    /// illegal ones are replaced with Void of the same total size.
    fn write_filtered(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.enabled {
            return self.inner.write(buf);
        }

        let mut pos = 0;

        while pos < buf.len() {
            // Try to read an element header at the current position
            let mut cursor = Cursor::new(&buf[pos..]);
            let header = match Header::read_from(&mut cursor) {
                Ok(h) => h,
                Err(_) => {
                    // Can't parse a valid header here — pass through remaining bytes
                    self.inner.write_all(&buf[pos..])?;
                    pos = buf.len();
                    break;
                }
            };

            let header_len = cursor.position() as usize;
            let element_id = header.id.as_encoded() as u32;
            let is_unknown = header.size.is_unknown;
            let data_size = *header.size as usize;

            if is_unknown {
                // Unknown-size elements (Segment, Cluster in streaming):
                // pass through header, continue scanning what follows
                self.inner.write_all(&buf[pos..pos + header_len])?;
                pos += header_len;
                continue;
            }

            let total_element_size = header_len + data_size;

            // Safety: if the element doesn't fit in the remaining buffer,
            // this isn't a real element boundary — just pass through
            if pos + total_element_size > buf.len() {
                self.inner.write_all(&buf[pos..])?;
                pos = buf.len();
                break;
            }

            if !is_webm_element(element_id) {
                // Illicit element: replace with Void, skip entirely (including children)
                trace!(
                    "WebM filter: voiding element 0x{:X} ({} data bytes)",
                    element_id, data_size
                );
                if let Some(void_bytes) = encode_void_element(total_element_size) {
                    self.inner.write_all(&void_bytes)?;
                } else {
                    // Element too small for Void; write zeros
                    let zeros = vec![0u8; total_element_size];
                    self.inner.write_all(&zeros)?;
                }
                pos += total_element_size;
            } else if is_master_element(element_id) {
                // Legal master element: write just the header, then continue
                // scanning into its children (they follow immediately)
                trace!(
                    "WebM filter: entering master 0x{:X} ({} data bytes)",
                    element_id, data_size
                );
                self.inner.write_all(&buf[pos..pos + header_len])?;
                pos += header_len;
            } else {
                // Legal leaf element: write header + data, skip past
                trace!(
                    "WebM filter: passing leaf 0x{:X} ({} data bytes)",
                    element_id, data_size
                );
                self.inner.write_all(&buf[pos..pos + total_element_size])?;
                pos += total_element_size;
            }
        }

        Ok(buf.len())
    }

    fn flush_inner(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ─── Public wrapper: Write ─────────────────────────────────────────────────

/// EBML-aware Write wrapper that filters non-WebM elements.
///
/// Wraps any `W: Write` and replaces non-whitelisted EBML elements with
/// `Void` elements of equal size.
///
/// When `enabled` is `false`, the wrapper is a transparent passthrough.
pub struct WebmFilterWriter<W: Write> {
    core: FilterCore<W>,
}

impl<W: Write> WebmFilterWriter<W> {
    /// Create a new WebM filter writer.
    ///
    /// - `inner`: the underlying writer
    /// - `enabled`: when `true`, non-WebM elements are voided; when `false`,
    ///   all writes pass through unmodified
    pub fn new(inner: W, enabled: bool) -> Self {
        Self {
            core: FilterCore::new(inner, enabled),
        }
    }

    /// Consume the wrapper and return the inner writer.
    pub fn into_inner(self) -> W {
        self.core.inner
    }

    /// Borrow the inner writer.
    pub fn inner(&self) -> &W {
        &self.core.inner
    }

    /// Mutably borrow the inner writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.core.inner
    }
}

impl<W: Write> Write for WebmFilterWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.core.write_filtered(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.core.flush_inner()
    }
}

/// Delegate `Seek` to the inner writer when it supports seeking.
impl<W: Write + Seek> Seek for WebmFilterWriter<W> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.core.inner.seek(pos)
    }
}

// ─── Public wrapper: Write + Send ──────────────────────────────────────────

/// EBML-aware `Write + Send` wrapper that filters non-WebM elements.
///
/// Identical in behavior to [`WebmFilterWriter`] but requires `Send` on the
/// inner writer, making it usable in `StreamSink` and other threaded contexts.
pub struct WebmFilterWriterSend<W: Write + Send> {
    core: FilterCore<W>,
}

impl<W: Write + Send> WebmFilterWriterSend<W> {
    /// Create a new WebM filter writer with `Send` bound.
    pub fn new(inner: W, enabled: bool) -> Self {
        Self {
            core: FilterCore::new(inner, enabled),
        }
    }

    /// Consume the wrapper and return the inner writer.
    pub fn into_inner(self) -> W {
        self.core.inner
    }

    /// Borrow the inner writer.
    pub fn inner(&self) -> &W {
        &self.core.inner
    }

    /// Mutably borrow the inner writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.core.inner
    }
}

impl<W: Write + Send> Write for WebmFilterWriterSend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.core.write_filtered(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.core.flush_inner()
    }
}

// Send is safe: the filter state is plain data and W is Send
unsafe impl<W: Write + Send> Send for WebmFilterWriterSend<W> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_webm_element_known_ids() {
        // Segment
        assert!(is_webm_element(0x18538067));
        // Cluster
        assert!(is_webm_element(0x1F43B675));
        // SimpleBlock
        assert!(is_webm_element(0xA3));
        // Tracks
        assert!(is_webm_element(0x1654AE6B));
        // Info
        assert!(is_webm_element(0x1549A966));
    }

    #[test]
    fn test_is_webm_element_non_webm() {
        // FieldOrder — not WebM
        assert!(!is_webm_element(0x9D));
        // Emphasis — not WebM
        assert!(!is_webm_element(0x52F1));
        // SegmentUUID — not WebM
        assert!(!is_webm_element(0x73A4));
    }

    #[test]
    fn test_whitelist_count() {
        assert!(
            WEBM_ELEMENT_COUNT > 100,
            "Expected >100 WebM elements, got {}",
            WEBM_ELEMENT_COUNT
        );
        assert!(
            WEBM_ELEMENT_COUNT < 200,
            "Expected <200 WebM elements, got {}",
            WEBM_ELEMENT_COUNT
        );
    }

    #[test]
    fn test_encode_void_element() {
        // Minimum void: 2 bytes (0xEC, 0x80 = Void with 0 payload)
        let v = encode_void_element(2).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], 0xEC);

        // 10-byte void
        let v = encode_void_element(10).unwrap();
        assert_eq!(v.len(), 10);
        assert_eq!(v[0], 0xEC);
    }

    #[test]
    fn test_filter_passthrough_when_disabled() {
        let inner = Vec::new();
        let mut writer = WebmFilterWriter::new(inner, false);
        let data = b"hello world";
        writer.write_all(data).unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.into_inner(), data);
    }

    #[test]
    fn test_filter_allows_webm_element() {
        // Timestamp (0xE7) with 2-byte payload
        // ID: 0xE7 (1 byte), Size: 0x82 (VInt = 2), Payload: [0x00, 0x01]
        let element = vec![0xE7, 0x82, 0x00, 0x01];

        let inner = Vec::new();
        let mut writer = WebmFilterWriter::new(inner, true);
        writer.write_all(&element).unwrap();
        writer.flush().unwrap();
        let output = writer.into_inner();
        assert_eq!(
            output, element,
            "WebM element should pass through unchanged"
        );
    }

    #[test]
    fn test_filter_voids_non_webm_element() {
        // FieldOrder (0x9D) with 1-byte payload
        // ID: 0x9D (1 byte), Size: 0x81 (VInt = 1), Payload: [0x02]
        let element = vec![0x9D, 0x81, 0x02];
        let total_len = element.len();

        let inner = Vec::new();
        let mut writer = WebmFilterWriter::new(inner, true);
        writer.write_all(&element).unwrap();
        writer.flush().unwrap();
        let output = writer.into_inner();

        assert_eq!(
            output.len(),
            total_len,
            "Voided element should have same total length"
        );
        assert_eq!(output[0], 0xEC, "First byte should be Void element ID");
    }

    #[test]
    fn test_filter_voided_followed_by_allowed() {
        // FieldOrder (0x9D) with 1-byte payload: [0x9D, 0x81, 0x02]
        // Then Timestamp (0xE7) with 2-byte payload: [0xE7, 0x82, 0x00, 0x01]
        let input = vec![0x9D, 0x81, 0x02, 0xE7, 0x82, 0x00, 0x01];
        let total_len = input.len();

        let inner = Vec::new();
        let mut writer = WebmFilterWriter::new(inner, true);
        writer.write_all(&input).unwrap();
        writer.flush().unwrap();
        let output = writer.into_inner();

        assert_eq!(
            output.len(),
            total_len,
            "Output must be exactly the same length as input"
        );
        // First 3 bytes should be a Void element
        assert_eq!(output[0], 0xEC, "First element should be voided");
        // Last 4 bytes should be the Timestamp element, unchanged
        assert_eq!(
            &output[3..],
            &[0xE7, 0x82, 0x00, 0x01],
            "Second element (Timestamp) must survive unchanged"
        );
    }

    #[test]
    fn test_filter_preserves_stream_length_with_multiple_elements() {
        // Timestamp (0xE7, WebM leaf): ID=0xE7, Size=0x82, data=[0x00, 0x01]
        // FieldOrder (0x9D, non-WebM): ID=0x9D, Size=0x81, data=[0x02]
        // SimpleBlock (0xA3, WebM leaf): ID=0xA3, Size=0x83, data=[0x81, 0x00, 0x00]
        let input: Vec<u8> = vec![
            0xE7, 0x82, 0x00, 0x01, // Timestamp
            0x9D, 0x81, 0x02, // FieldOrder
            0xA3, 0x83, 0x81, 0x00, 0x00, // SimpleBlock
        ];

        let inner = Vec::new();
        let mut writer = WebmFilterWriter::new(inner, true);
        writer.write_all(&input).unwrap();
        writer.flush().unwrap();
        let output = writer.into_inner();

        assert_eq!(
            output.len(),
            input.len(),
            "Total output length must match input length"
        );
        // Timestamp should be untouched
        assert_eq!(&output[0..4], &[0xE7, 0x82, 0x00, 0x01]);
        // FieldOrder should be voided
        assert_eq!(output[4], 0xEC);
        // SimpleBlock should be untouched
        assert_eq!(&output[7..12], &[0xA3, 0x83, 0x81, 0x00, 0x00]);
    }
}
