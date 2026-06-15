mod block_ext;
mod cluster_warpper;
mod error;
pub mod folder_streamer;
mod metling_pot;
mod remuxer;
pub mod sink;
pub mod source;
mod source_mappings;
pub mod test_utils;

// Re-exports
pub use block_ext::ClusterBlockExt;
pub use cluster_warpper::{ClusterReadWrapper, ClusterWriteWrapper};
pub use error::{Error, Result};
pub use metling_pot::MeltingPot;
pub use mkv_element;
pub use remuxer::{
    ChunkedRemuxer, ChunkedRemuxerResponse, RemuxStats, Remuxer, RemuxerCutMode, RemuxerState,
    TrackMapping, remux,
};
pub use sink::{ChunkedSinkHandle, ChunkedStreamSink, FileSink, Sink};
pub use source::util::basic_info::MkvBasicInfo;
pub use source::{CutInterval, Cutting, InputSource, MkvReader, Remuxing, SeekType, Source};
pub use source_mappings::SourcesMappings;
pub use test_utils::{
    MkvValidationReport, ValidationStats, get_input_duration_ns, validate_mkv_output,
};

use log::warn;
use mkv_element::prelude::*;

const APP_NAME: &str = env!("CARGO_PKG_NAME");

/// Selects whether the output container should be WebM or Matroska (MKV).
///
/// WebM is a restricted subset of Matroska that only allows specific codecs
/// (VP8, VP9, AV1, Vorbis, Opus, WebVTT) and requires `DocType: webm` in the
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerFormat {
    Mkv,
    WebM,
    Vtt,
}

/// A list of codec names to use for the output container.
/// Stores both the Matroska codec ID strings and the original `TrackEntry`
/// objects so that codec-specific MIME parameters (e.g. AV1 profile/level)
/// can be derived from the actual track metadata.
#[derive(Debug, Clone)]
pub struct Codecs {
    /// Deduplicated (codec_id, representative TrackEntry) pairs, sorted by
    /// type priority (video → audio → subtitle) then alphabetically.
    inner: Vec<(String, TrackEntry)>,
}

/// Returns a sort-priority for a Matroska codec ID so that video codecs
/// come first, then audio, then subtitle/data codecs.
fn codec_type_priority(codec_id: &str) -> u8 {
    match codec_id.get(..2) {
        Some("V_") => 0,
        Some("A_") => 1,
        Some("S_") | Some("D_") => 2,
        _ => 3,
    }
}

impl Codecs {
    pub(crate) fn new(tracks: &Tracks) -> Self {
        let mut entries: Vec<(String, TrackEntry)> = Vec::new();
        for track in &tracks.track_entry {
            let codec_id = track.codec_id.0.to_string();
            entries.push((codec_id, track.clone()));
        }
        // Sort by type (video → audio → subtitle) then alphabetically, and deduplicate
        // by codec_id (keep first TrackEntry encountered for each unique codec_id).
        entries.sort_by(|(a, _), (b, _)| {
            codec_type_priority(a)
                .cmp(&codec_type_priority(b))
                .then_with(|| a.cmp(b))
        });
        entries.dedup_by(|(a, _), (b, _)| a == b);
        Self { inner: entries }
    }
}

/// Build the AV1 codec string from the AV1CodecConfigurationRecord stored
/// in `CodecPrivate` and from the Matroska `Video > Colour` element.
///
/// Format (per <https://aomediacodec.github.io/av1-isobmff/#codecsparam>):
///   `av01.<P>.<LLT>.<DD>[.<M>.<CCC>.<cp>.<tc>.<mc>.<F>]`
///
/// Falls back to the conservative default `av01.0.19H.10` if the CodecPrivate
/// is missing or too short to parse.
fn av1_codec_string(track: &TrackEntry) -> String {
    const FALLBACK: &str = "av01.0.19H.10";

    let codec_private = match track.codec_private.as_ref() {
        Some(cp) => &cp.0,
        None => {
            warn!("AV1 track has no CodecPrivate – using fallback codec string");
            return FALLBACK.into();
        }
    };

    // AV1CodecConfigurationRecord is at least 4 bytes.
    if codec_private.len() < 4 {
        warn!(
            "AV1 CodecPrivate too short ({} bytes) – using fallback codec string",
            codec_private.len()
        );
        return FALLBACK.into();
    }

    // ── Parse the AV1CodecConfigurationRecord ────────────────────────
    // Byte 0: marker(1) | version(7)
    // Byte 1: seq_profile(3) | seq_level_idx_0(5)
    // Byte 2: seq_tier_0(1) | high_bitdepth(1) | twelve_bit(1) | monochrome(1)
    //         | chroma_subsampling_x(1) | chroma_subsampling_y(1) | chroma_sample_position(2)
    // Byte 3: reserved(3) | initial_presentation_delay_present(1) | ...(4)
    let byte1 = codec_private[1];
    let byte2 = codec_private[2];

    let seq_profile = (byte1 >> 5) & 0x07;
    let seq_level_idx = byte1 & 0x1F;
    let seq_tier = (byte2 >> 7) & 0x01;
    let high_bitdepth = (byte2 >> 6) & 0x01;
    let twelve_bit = (byte2 >> 5) & 0x01;
    let monochrome = (byte2 >> 4) & 0x01;
    let chroma_subsampling_x = (byte2 >> 3) & 0x01;
    let chroma_subsampling_y = (byte2 >> 2) & 0x01;
    let chroma_sample_position = byte2 & 0x03;

    // Derive bit depth per the AV1 spec.
    let bit_depth: u8 = if high_bitdepth != 0 {
        if twelve_bit != 0 { 12 } else { 10 }
    } else {
        8
    };

    let tier_char = if seq_tier == 0 { 'M' } else { 'H' };

    // chromaSubsampling = "{subsampling_x}{subsampling_y}{chroma_sample_position}"
    let chroma_subsampling = format!(
        "{}{}{}",
        chroma_subsampling_x, chroma_subsampling_y, chroma_sample_position
    );

    // Colour description from the Matroska Colour element (defaults per CICP
    // when absent: BT.709 = 1/1/1, studio swing = 0).
    let (color_primaries, transfer_characteristics, matrix_coefficients, video_full_range) =
        if let Some(video) = track.video.as_ref() {
            if let Some(colour) = video.colour.as_ref() {
                let cp = colour.primaries.0;
                let tc = colour.transfer_characteristics.0;
                let mc = colour.matrix_coefficients.0;
                // Matroska Range: 0=unspecified, 1=broadcast, 2=full, 3=derived
                // AV1 videoFullRangeFlag: 0=studio/limited, 1=full
                let vfr = if colour.range.0 == 2 { 1u64 } else { 0u64 };
                (cp, tc, mc, vfr)
            } else {
                (1u64, 1u64, 1u64, 0u64)
            }
        } else {
            (1u64, 1u64, 1u64, 0u64)
        };

    // Build the full codec string.
    // All numeric fields after the 4CC are zero-padded to 2 digits, except
    // videoFullRangeFlag which is a single digit.
    format!(
        "av01.{}.{:02}{}.{:02}.{}.{}.{:02}.{:02}.{:02}.{}",
        seq_profile,
        seq_level_idx,
        tier_char,
        bit_depth,
        monochrome,
        chroma_subsampling,
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        video_full_range,
    )
}

/// Convert a Matroska `TrackEntry` to the ISO-standard MIME codec name used
/// in the `codecs` parameter.
///
/// For AV1 tracks the codec string is derived from the actual track metadata
/// (CodecPrivate + Colour element).  Other codecs use fixed mappings.
/// Subtitle/data codecs (`D_`, `S_`) are skipped since they aren't relevant
/// for the MIME codecs parameter.
fn track_entry_to_mime(track: &TrackEntry) -> Option<String> {
    let codec_id = track.codec_id.0.as_str();

    // Explicit mappings for known codecs.
    match codec_id {
        "V_AV1" => return Some(av1_codec_string(track)),
        "V_VP9" => return Some("vp9".into()),
        "V_VP8" => return Some("vp8".into()),
        "A_OPUS" => return Some("opus".into()),
        "A_VORBIS" => return Some("vorbis".into()),
        _ => {}
    }
    // Fallback: strip the single-letter prefix + underscore and lowercase
    let without_prefix = match codec_id.get(..2) {
        Some("V_") | Some("A_") => &codec_id[2..],
        Some("D_") | Some("S_") => return None, // skip subtitle/data codecs
        _ => return None,
    };
    // Take only the major codec ID (before any '/' suffix)
    let major = without_prefix.split('/').next().unwrap_or(without_prefix);
    Some(major.to_lowercase())
}

/// Implement conversion for Mkv codec_id to MIME type
impl Codecs {
    pub fn to_mime_type(&self, container_format: ContainerFormat) -> String {
        let prefix = match container_format {
            ContainerFormat::Vtt => return "text/vtt; charset=utf-8".to_string(),
            ContainerFormat::WebM => "video/webm",
            ContainerFormat::Mkv => "video/x-matroska",
        };
        let mime_codecs: Vec<String> = self
            .inner
            .iter()
            .filter_map(|(_, te)| track_entry_to_mime(te))
            .collect();
        if mime_codecs.is_empty() {
            prefix.to_string()
        } else {
            format!("{}; codecs=\"{}\"", prefix, mime_codecs.join(","))
        }
    }

    pub fn get_mkv_codec_ids(&self) -> Vec<String> {
        self.inner.iter().map(|(id, _)| id.clone()).collect()
    }

    pub fn get_mime_codec_ids(&self) -> Vec<String> {
        self.inner
            .iter()
            .filter_map(|(_, te)| track_entry_to_mime(te))
            .collect()
    }
}

impl std::fmt::Display for ContainerFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerFormat::Mkv => write!(f, "matroska"),
            ContainerFormat::WebM => write!(f, "webm"),
            ContainerFormat::Vtt => write!(f, "webvtt"),
        }
    }
}

impl std::str::FromStr for ContainerFormat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "mkv" | "matroska" => Ok(ContainerFormat::Mkv),
            "webm" => Ok(ContainerFormat::WebM),
            "webvtt" => Ok(ContainerFormat::Vtt),
            _ => Err(Error::UnsupportedCodec {
                codec_id: s.to_string(),
                reason: "must be either 'mkv', 'webm', or 'webvtt'".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{FileSource, InputSource};
    use std::fs::File;
    use std::path::PathBuf;

    /// Helper: build a minimal `TrackEntry` stub carrying only a `CodecId`.
    /// Used by unit tests that don't need real AV1 CodecPrivate data.
    fn dummy_track_entry(codec_id: &str) -> TrackEntry {
        let mut te = TrackEntry::default();
        te.codec_id = CodecId(codec_id.to_string());
        te
    }

    /// Helper: build a Codecs from raw Matroska codec ID strings.
    /// Creates dummy `TrackEntry` stubs so unit tests compile without real
    /// track metadata.
    fn codecs_from_raw(ids: &[&str]) -> Codecs {
        let mut entries: Vec<(String, TrackEntry)> = ids
            .iter()
            .map(|s| (s.to_string(), dummy_track_entry(s)))
            .collect();
        entries.sort_by(|(a, _), (b, _)| {
            codec_type_priority(a)
                .cmp(&codec_type_priority(b))
                .then_with(|| a.cmp(b))
        });
        entries.dedup_by(|(a, _), (b, _)| a == b);
        Codecs { inner: entries }
    }

    /// Helper: read the Tracks element from a test file and build Codecs.
    fn codecs_from_file(name: &str) -> Codecs {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
        let file = File::open(&path).expect("Failed to open test file");
        let source: InputSource<crate::source::Uninitialized> =
            FileSource::new(file).unwrap().into();
        let source = source.initialize(None).unwrap();
        let tracks = source.get_tracks().unwrap();
        Codecs::new(&tracks)
    }

    // ── track_entry_to_mime unit tests ──────────────────────────────────

    #[test]
    fn test_track_entry_to_mime_video() {
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("V_VP9")),
            Some("vp9".into())
        );
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("V_VP8")),
            Some("vp8".into())
        );
        // AV1 without CodecPrivate falls back to the conservative default
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("V_AV1")),
            Some("av01.0.19H.10".into())
        );
    }

    #[test]
    fn test_track_entry_to_mime_audio() {
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("A_OPUS")),
            Some("opus".into())
        );
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("A_VORBIS")),
            Some("vorbis".into())
        );
    }

    #[test]
    fn test_track_entry_to_mime_strips_suffix() {
        // Only the major codec ID should be kept
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("V_MPEG4/ISO/ASP")),
            Some("mpeg4".into())
        );
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("A_AAC/MPEG2/LC/SBR")),
            Some("aac".into())
        );
    }

    #[test]
    fn test_track_entry_to_mime_skips_subtitle_and_data() {
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("D_WEBVTT/SUBTITLES")),
            None
        );
        assert_eq!(
            track_entry_to_mime(&dummy_track_entry("D_WEBVTT/CAPTIONS")),
            None
        );
        assert_eq!(track_entry_to_mime(&dummy_track_entry("S_TEXT/UTF8")), None);
    }

    #[test]
    fn test_track_entry_to_mime_unknown_prefix() {
        assert_eq!(track_entry_to_mime(&dummy_track_entry("X_SOMETHING")), None);
        assert_eq!(track_entry_to_mime(&dummy_track_entry("")), None);
        assert_eq!(track_entry_to_mime(&dummy_track_entry("V")), None);
    }

    // ── av1_codec_string unit tests ────────────────────────────────────

    #[test]
    fn test_av1_codec_string_parses_config_record() {
        use bytes::Bytes;
        // Construct a synthetic AV1CodecConfigurationRecord:
        // Profile 0, Level 4 (seq_level_idx=4), Main tier, 10-bit, non-mono,
        // 4:2:0 chroma (subsampling_x=1, subsampling_y=1), sample_position=0
        let mut te = dummy_track_entry("V_AV1");
        // Byte 0: marker=1 (0x80) | version=1 => 0x81
        // Byte 1: seq_profile=0 (0b000) | seq_level_idx=4 (0b00100) => 0x04
        // Byte 2: seq_tier=0 | high_bitdepth=1 | twelve_bit=0 | monochrome=0
        //         | chroma_subsampling_x=1 | chroma_subsampling_y=1 | chroma_sample_position=0
        //       = 0b01001100 => 0x4C
        // Byte 3: reserved=0, no delay => 0x00
        te.codec_private = Some(CodecPrivate(Bytes::from_static(&[0x81, 0x04, 0x4C, 0x00])));
        // Add Colour metadata: BT.709 (1/1/1), studio range
        te.video = Some(Video {
            colour: Some(Colour {
                primaries: Primaries(1),
                transfer_characteristics: TransferCharacteristics(1),
                matrix_coefficients: MatrixCoefficients(1),
                range: Range(1), // broadcast = studio swing
                ..Colour::default()
            }),
            ..Video::default()
        });

        let result = av1_codec_string(&te);
        // av01.0.04M.10.0.110.01.01.01.0
        assert_eq!(result, "av01.0.04M.10.0.110.01.01.01.0");
    }

    #[test]
    fn test_av1_codec_string_hdr_content() {
        use bytes::Bytes;
        // Profile 0, Level 13 (4.1), High tier, 10-bit, non-mono,
        // 4:2:0 (sub_x=1, sub_y=1, pos=0), BT.2020 primaries (9),
        // PQ transfer (16), BT.2020 matrix (9), full range
        let mut te = dummy_track_entry("V_AV1");
        // Byte 1: profile=0, level=13 => 0x0D
        // Byte 2: tier=1 | high_bitdepth=1 | twelve_bit=0 | mono=0
        //         | sub_x=1 | sub_y=1 | pos=0
        //       = 0b11001100 => 0xCC
        te.codec_private = Some(CodecPrivate(Bytes::from_static(&[0x81, 0x0D, 0xCC, 0x00])));
        te.video = Some(Video {
            colour: Some(Colour {
                primaries: Primaries(9),
                transfer_characteristics: TransferCharacteristics(16),
                matrix_coefficients: MatrixCoefficients(9),
                range: Range(2), // full range
                ..Colour::default()
            }),
            ..Video::default()
        });

        let result = av1_codec_string(&te);
        assert_eq!(result, "av01.0.13H.10.0.110.09.16.09.1");
    }

    #[test]
    fn test_av1_codec_string_no_codec_private_fallback() {
        let te = dummy_track_entry("V_AV1");
        assert_eq!(av1_codec_string(&te), "av01.0.19H.10");
    }

    // ── Codecs::to_mime_type unit tests ─────────────────────────────────

    #[test]
    fn test_to_mime_type_webm_vp9_opus() {
        let codecs = codecs_from_raw(&["V_VP9", "A_OPUS"]);
        let mime = codecs.to_mime_type(ContainerFormat::WebM);
        assert_eq!(mime, "video/webm; codecs=\"vp9,opus\"");
    }

    #[test]
    fn test_to_mime_type_webm_av1_opus_fallback() {
        // With dummy TrackEntries (no CodecPrivate), AV1 falls back to default
        let codecs = codecs_from_raw(&["V_AV1", "A_OPUS"]);
        let mime = codecs.to_mime_type(ContainerFormat::WebM);
        assert_eq!(mime, "video/webm; codecs=\"av01.0.19H.10,opus\"");
    }

    #[test]
    fn test_to_mime_type_mkv() {
        let codecs = codecs_from_raw(&["V_VP9", "A_OPUS"]);
        let mime = codecs.to_mime_type(ContainerFormat::Mkv);
        assert_eq!(mime, "video/x-matroska; codecs=\"vp9,opus\"");
    }

    #[test]
    fn test_to_mime_type_vtt_ignores_codecs() {
        let codecs = codecs_from_raw(&["D_WEBVTT/SUBTITLES"]);
        let mime = codecs.to_mime_type(ContainerFormat::Vtt);
        assert_eq!(mime, "text/vtt; charset=utf-8");
    }

    #[test]
    fn test_to_mime_type_subtitle_only_no_codecs_param() {
        // When all codecs are subtitle/data, the codecs param should be omitted
        let codecs = codecs_from_raw(&["D_WEBVTT/SUBTITLES", "D_WEBVTT/CAPTIONS"]);
        let mime = codecs.to_mime_type(ContainerFormat::WebM);
        assert_eq!(mime, "video/webm");
    }

    // ── Integration tests with real test files ──────────────────────────

    #[test]
    fn test_mime_type_from_test_av1_webm() {
        let codecs = codecs_from_file("test_av1.webm");

        // test_av1.webm has: AV1 video, Opus audio, and a WebVTT subtitle track
        assert_eq!(
            codecs.get_mkv_codec_ids(),
            vec!["V_AV1", "A_OPUS", "D_WEBVTT/SUBTITLES"]
        );
        // The AV1 codec string is now derived from the file's actual metadata
        let mime_ids = codecs.get_mime_codec_ids();
        assert_eq!(mime_ids.len(), 2);
        assert!(
            mime_ids[0].starts_with("av01."),
            "AV1 MIME codec should start with 'av01.', got: {}",
            mime_ids[0]
        );
        assert_eq!(mime_ids[1], "opus");

        let mime = codecs.to_mime_type(ContainerFormat::WebM);
        assert!(
            mime.starts_with("video/webm; codecs=\"av01."),
            "Full MIME type should contain av01 codec string, got: {}",
            mime
        );
        assert!(
            mime.contains(",opus\""),
            "Full MIME type should contain opus, got: {}",
            mime
        );
    }

    #[test]
    fn test_mime_type_from_test_vp9_webm() {
        let codecs = codecs_from_file("test_vp9.webm");

        // test_vp9.webm has: VP9 video, Opus audio, and a WebVTT subtitle track
        assert_eq!(
            codecs.get_mkv_codec_ids(),
            vec!["V_VP9", "A_OPUS", "D_WEBVTT/SUBTITLES"]
        );
        assert_eq!(codecs.get_mime_codec_ids(), vec!["vp9", "opus"]);
        assert_eq!(
            codecs.to_mime_type(ContainerFormat::WebM),
            "video/webm; codecs=\"vp9,opus\""
        );
    }
}
