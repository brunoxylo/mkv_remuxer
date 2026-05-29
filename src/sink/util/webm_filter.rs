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

use log::trace;
use std::io::{self, Write};

// Pull in the build-time generated whitelist
mod webm_whitelist {
    include!(concat!(env!("OUT_DIR"), "/webm_whitelist.rs"));
}
pub use webm_whitelist::{WEBM_ELEMENT_COUNT, WEBM_ELEMENT_IDS, is_webm_element};

// ─── EBML VInt helpers ──────────────────────────────────────────────────────

/// Decode an EBML variable-length integer (VInt) from the front of `buf`.
/// Returns `(decoded_value_with_marker_bit_masked, byte_length)` or `None`
/// if the buffer is too short.
fn decode_vint(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.is_empty() {
        return None;
    }
    let first = buf[0];
    if first == 0 {
        return None; // invalid
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || buf.len() < len {
        return None;
    }
    let mut value = (first as u64) & (0xFF >> len); // mask off the marker bit
    for &b in &buf[1..len] {
        value = (value << 8) | b as u64;
    }
    Some((value, len))
}

/// Decode an EBML element ID from the front of `buf`.
/// Unlike data VInts, element IDs keep the leading marker bit.
/// Returns `(id_as_u32, byte_length)` or `None`.
fn decode_element_id(buf: &[u8]) -> Option<(u32, usize)> {
    if buf.is_empty() {
        return None;
    }
    let first = buf[0];
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || buf.len() < len {
        return None;
    }
    // Element IDs include the marker bit
    let mut value = first as u32;
    for &b in &buf[1..len] {
        value = (value << 8) | b as u32;
    }
    Some((value, len))
}

/// Encode a Void EBML element that occupies exactly `total_bytes` on the wire.
/// Returns `None` if `total_bytes` is too small (< 2 bytes for the shortest Void).
fn encode_void_element(total_bytes: usize) -> Option<Vec<u8>> {
    if total_bytes < 2 {
        return None;
    }
    // Void element ID is 0xEC (1 byte).
    // We need: 1 (ID) + vint_len (size) + payload = total_bytes
    // Payload is all zeros.
    //
    // Try increasing VInt sizes until we get an exact fit.
    for vint_len in 1..=8usize {
        let payload = total_bytes.checked_sub(1 + vint_len)?;
        // Check if payload fits in vint_len bytes
        let max_payload = if vint_len == 8 {
            u64::MAX >> 1
        } else {
            (1u64 << (7 * vint_len)) - 2 // -1 for the "unknown size" reservation, -1 for max
        };
        if payload as u64 <= max_payload {
            let mut buf = Vec::with_capacity(total_bytes);
            buf.push(0xEC); // Void element ID
            // Encode the size as a VInt of exactly `vint_len` bytes
            let marker = 1u64 << (8 * vint_len - vint_len); // e.g. 0x80 for len=1
            let encoded = marker | (payload as u64);
            for i in (0..vint_len).rev() {
                buf.push((encoded >> (8 * i)) as u8);
            }
            buf.resize(total_bytes, 0x00); // zero-filled payload
            return Some(buf);
        }
    }
    None
}

/// Check whether a VInt-encoded size represents "unknown size"
/// (all data bits set to 1).
fn is_unknown_size(value: u64, vint_len: usize) -> bool {
    let all_ones = (1u64 << (7 * vint_len)) - 1;
    value == all_ones
}

// ─── Filtering state machine ───────────────────────────────────────────────

/// Internal state for the EBML stream parser.
#[derive(Debug)]
enum FilterState {
    /// Passthrough: we're copying bytes to the inner writer.
    Passthrough,
    /// We're inside an element that must be voided out.
    /// `remaining` is the number of payload bytes left to consume (and discard).
    Voiding { remaining: u64 },
}

/// Core filter logic, generic over `W` but without any trait bounds itself
/// so we can share the implementation between the two public wrappers.
struct FilterCore<W> {
    inner: W,
    state: FilterState,
    /// Small buffer to accumulate a complete element header (ID + size).
    /// Max element header is 4 (ID) + 8 (VInt size) = 12 bytes.
    header_buf: Vec<u8>,
    /// Whether filtering is enabled at all. When `false`, all writes pass
    /// through without inspection (used for MKV output).
    enabled: bool,
}

impl<W> FilterCore<W> {
    fn new(inner: W, enabled: bool) -> Self {
        Self {
            inner,
            state: FilterState::Passthrough,
            header_buf: Vec::with_capacity(12),
            enabled,
        }
    }
}

impl<W: Write> FilterCore<W> {
    fn write_filtered(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.enabled {
            return self.inner.write(buf);
        }

        let mut consumed = 0usize;

        while consumed < buf.len() {
            match &mut self.state {
                FilterState::Voiding { remaining } => {
                    // Discard bytes that belong to the voided element's payload
                    let to_skip = (*remaining as usize).min(buf.len() - consumed);
                    *remaining -= to_skip as u64;
                    consumed += to_skip;

                    if *remaining == 0 {
                        self.state = FilterState::Passthrough;
                    }
                }

                FilterState::Passthrough => {
                    // If we have a partial header buffered, keep accumulating
                    if !self.header_buf.is_empty() {
                        // We need up to 12 bytes total for a full header
                        let needed = 12usize.saturating_sub(self.header_buf.len());
                        let available = (buf.len() - consumed).min(needed);
                        self.header_buf.extend_from_slice(&buf[consumed..consumed + available]);
                        consumed += available;

                        // Try to parse a complete header from the buffer
                        match self.try_parse_header() {
                            HeaderParse::NeedMore => {
                                // Still not enough bytes — we'll get more in the next write
                                continue;
                            }
                            HeaderParse::Allowed { header_len } => {
                                // Element is WebM-compatible: flush the buffered header
                                self.inner.write_all(&self.header_buf[..header_len])?;
                                // Any excess bytes were over-read; put them back
                                let excess = self.header_buf[header_len..].to_vec();
                                self.header_buf.clear();
                                // Re-process the excess bytes
                                if !excess.is_empty() {
                                    // Push excess back by adjusting consumed
                                    consumed -= excess.len();
                                }
                            }
                            HeaderParse::Voided { header_len, data_size } => {
                                let total_element_size = header_len as u64 + data_size;
                                // Write a Void element of the same total size
                                if let Some(void_bytes) = encode_void_element(total_element_size as usize) {
                                    self.inner.write_all(&void_bytes)?;
                                } else {
                                    // Element is too small for Void; just write zeros
                                    let zeros = vec![0u8; total_element_size as usize];
                                    self.inner.write_all(&zeros)?;
                                }
                                self.state = FilterState::Voiding { remaining: data_size };
                                // Any excess bytes were over-read; put them back
                                let excess = self.header_buf[header_len..].to_vec();
                                self.header_buf.clear();
                                if !excess.is_empty() {
                                    consumed -= excess.len();
                                }
                            }
                            HeaderParse::NotAnElement => {
                                // Not a valid EBML header — just pass through the first byte
                                // and try again
                                self.inner.write_all(&self.header_buf[..1])?;
                                let rest = self.header_buf[1..].to_vec();
                                self.header_buf.clear();
                                if !rest.is_empty() {
                                    self.header_buf.extend_from_slice(&rest);
                                }
                            }
                        }
                    } else {
                        // No partial header — start fresh.
                        // Peek at the current byte to decide: is this the start of an
                        // element header?
                        let b = buf[consumed];
                        if b == 0 {
                            // Zero byte — just pass through
                            self.inner.write_all(&[b])?;
                            consumed += 1;
                        } else {
                            // Potential element header start — buffer it
                            self.header_buf.push(b);
                            consumed += 1;
                        }
                    }
                }
            }
        }

        // We always "consume" the entire input from the caller's perspective
        Ok(buf.len())
    }

    fn flush_inner(&mut self) -> io::Result<()> {
        // If we have pending header bytes, flush them through (assume passthrough)
        if !self.header_buf.is_empty() {
            self.inner.write_all(&self.header_buf)?;
            self.header_buf.clear();
        }
        self.inner.flush()
    }

    /// Try to parse a complete EBML element header from `self.header_buf`.
    fn try_parse_header(&self) -> HeaderParse {
        let buf = &self.header_buf;

        // 1. Decode element ID
        let (element_id, id_len) = match decode_element_id(buf) {
            Some(v) => v,
            None => {
                if buf.len() < 4 {
                    return HeaderParse::NeedMore;
                } else {
                    return HeaderParse::NotAnElement;
                }
            }
        };

        // 2. Decode data size VInt
        if buf.len() <= id_len {
            return HeaderParse::NeedMore;
        }
        let size_buf = &buf[id_len..];
        let (data_size, size_len) = match decode_vint(size_buf) {
            Some(v) => v,
            None => {
                if buf.len() < id_len + 8 {
                    return HeaderParse::NeedMore;
                } else {
                    return HeaderParse::NotAnElement;
                }
            }
        };

        let header_len = id_len + size_len;

        // Handle unknown-size elements (e.g., Segment, Cluster in streaming mode).
        // These are always passed through regardless of whitelist status, because
        // voiding an unknown-size element is not meaningful.
        if is_unknown_size(data_size, size_len) {
            return HeaderParse::Allowed { header_len };
        }

        // 3. Check whitelist
        if is_webm_element(element_id) {
            trace!("WebM filter: allowing element 0x{:X} ({} data bytes)", element_id, data_size);
            HeaderParse::Allowed { header_len }
        } else {
            trace!("WebM filter: voiding element 0x{:X} ({} data bytes)", element_id, data_size);
            HeaderParse::Voided { header_len, data_size }
        }
    }
}

enum HeaderParse {
    /// Not enough bytes to determine the header yet.
    NeedMore,
    /// Valid header for a WebM-allowed element; pass it through.
    Allowed { header_len: usize },
    /// Valid header for a non-WebM element; void it out.
    Voided { header_len: usize, data_size: u64 },
    /// Not a valid EBML element header.
    NotAnElement,
}

// ─── Public wrapper: Write (covers ?Sized through dyn Write) ────────────────

/// EBML-aware Write wrapper that filters non-WebM elements.
///
/// Wraps any `W: Write` (including `dyn Write` via `Box<dyn Write>`,
/// `BufWriter<File>`, etc.) and replaces non-whitelisted EBML elements with
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
        // Sanity check: we should have a reasonable number of WebM elements
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
    fn test_decode_element_id() {
        // 1-byte ID: 0xA3 (SimpleBlock)
        assert_eq!(decode_element_id(&[0xA3]), Some((0xA3, 1)));
        // 2-byte ID: 0x4DBB (Seek)
        assert_eq!(decode_element_id(&[0x4D, 0xBB]), Some((0x4DBB, 2)));
        // 4-byte ID: 0x18538067 (Segment)
        assert_eq!(
            decode_element_id(&[0x18, 0x53, 0x80, 0x67]),
            Some((0x18538067, 4))
        );
    }

    #[test]
    fn test_decode_vint() {
        // 1-byte VInt: 0x81 = value 1
        assert_eq!(decode_vint(&[0x81]), Some((1, 1)));
        // 1-byte VInt: 0x85 = value 5
        assert_eq!(decode_vint(&[0x85]), Some((5, 1)));
        // 2-byte VInt: 0x40 0x02 = value 2
        assert_eq!(decode_vint(&[0x40, 0x02]), Some((2, 2)));
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
        // Construct a valid WebM element: Timestamp (0xE7) with 2-byte payload
        // ID: 0xE7 (1 byte), Size: 0x82 (VInt = 2), Payload: [0x00, 0x01]
        let element = vec![0xE7, 0x82, 0x00, 0x01];

        let inner = Vec::new();
        let mut writer = WebmFilterWriter::new(inner, true);
        writer.write_all(&element).unwrap();
        writer.flush().unwrap();
        let output = writer.into_inner();
        assert_eq!(output, element, "WebM element should pass through unchanged");
    }

    #[test]
    fn test_filter_voids_non_webm_element() {
        // Construct a non-WebM element: FieldOrder (0x9D) with 1-byte payload
        // ID: 0x9D (1 byte), Size: 0x81 (VInt = 1), Payload: [0x02]
        let element = vec![0x9D, 0x81, 0x02];
        let total_len = element.len();

        let inner = Vec::new();
        let mut writer = WebmFilterWriter::new(inner, true);
        writer.write_all(&element).unwrap();
        writer.flush().unwrap();
        let output = writer.into_inner();

        // Should be same length but replaced with Void
        assert_eq!(output.len(), total_len, "Voided element should have same total length");
        assert_eq!(output[0], 0xEC, "First byte should be Void element ID");
    }
}
