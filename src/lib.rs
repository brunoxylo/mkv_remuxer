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
pub use remuxer::{RemuxStats, Remuxer, RemuxerCutMode, RemuxerState, TrackMapping, remux};
pub use sink::{FileSink, Sink};
pub use source::util::basic_info::MkvBasicInfo;
pub use source::{CutInterval, Cutting, InputSource, MkvReader, Remuxing, SeekType, Source};
pub use source_mappings::SourcesMappings;
pub use test_utils::{
    MkvValidationReport, ValidationStats, get_input_duration_ns, validate_mkv_output,
};

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
#[derive(Debug, Clone)]
pub struct Codecs {
    inner: Vec<String>,
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
        let mut codecs = Vec::new();
        for track in &tracks.track_entry {
            codecs.push(track.codec_id.0.to_string());
        }
        // Sort by type (video → audio → subtitle) then alphabetically, and deduplicate.
        codecs.sort_by(|a, b| {
            codec_type_priority(a)
                .cmp(&codec_type_priority(b))
                .then_with(|| a.cmp(b))
        });
        codecs.dedup();
        Self { inner: codecs }
    }
}

/// Convert a Matroska codec ID (e.g. `V_VP9`, `A_OPUS`, `D_WEBVTT/SUBTITLES`)
/// to the ISO-standard MIME codec name used in the `codecs` parameter.
/// Known codecs are mapped explicitly (e.g. `V_AV1` → `av01.0.01M.08`).
/// Subtitle/data codecs (`D_`, `S_`) are skipped since they aren't relevant for
/// the MIME codecs parameter.
fn mkv_codec_id_to_mime(codec_id: &str) -> Option<String> {
    // Explicit mappings for known codecs to their ISO-standard identifiers.
    // AV1 requires profile/level/bit-depth for MSE isTypeSupported() to accept it.
    // We use the most conservative params: Main profile, Level 2.1, Main tier, 8-bit.
    match codec_id {
        "V_AV1" => return Some("av01.0.01M.08".into()),
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
            .filter_map(|c| mkv_codec_id_to_mime(c))
            .collect();
        if mime_codecs.is_empty() {
            prefix.to_string()
        } else {
            format!("{}; codecs=\"{}\"", prefix, mime_codecs.join(", "))
        }
    }

    pub fn get_mkv_codec_ids(&self) -> Vec<String> {
        self.inner.clone()
    }

    pub fn get_mime_codec_ids(&self) -> Vec<String> {
        let mime_codecs: Vec<String> = self
            .inner
            .iter()
            .filter_map(|c| mkv_codec_id_to_mime(c))
            .collect();
        mime_codecs
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

    /// Helper: build a Codecs from raw Matroska codec ID strings.
    fn codecs_from_raw(ids: &[&str]) -> Codecs {
        let mut inner: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        inner.sort_by(|a, b| {
            codec_type_priority(a)
                .cmp(&codec_type_priority(b))
                .then_with(|| a.cmp(b))
        });
        inner.dedup();
        Codecs { inner }
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

    // ── mkv_codec_id_to_mime unit tests ─────────────────────────────────

    #[test]
    fn test_mkv_codec_id_to_mime_video() {
        assert_eq!(mkv_codec_id_to_mime("V_VP9"), Some("vp9".into()));
        assert_eq!(mkv_codec_id_to_mime("V_VP8"), Some("vp8".into()));
        assert_eq!(mkv_codec_id_to_mime("V_AV1"), Some("av01.0.01M.08".into()));
    }

    #[test]
    fn test_mkv_codec_id_to_mime_audio() {
        assert_eq!(mkv_codec_id_to_mime("A_OPUS"), Some("opus".into()));
        assert_eq!(mkv_codec_id_to_mime("A_VORBIS"), Some("vorbis".into()));
    }

    #[test]
    fn test_mkv_codec_id_to_mime_strips_suffix() {
        // Only the major codec ID should be kept
        assert_eq!(mkv_codec_id_to_mime("V_MPEG4/ISO/ASP"), Some("mpeg4".into()));
        assert_eq!(mkv_codec_id_to_mime("A_AAC/MPEG2/LC/SBR"), Some("aac".into()));
    }

    #[test]
    fn test_mkv_codec_id_to_mime_skips_subtitle_and_data() {
        assert_eq!(mkv_codec_id_to_mime("D_WEBVTT/SUBTITLES"), None);
        assert_eq!(mkv_codec_id_to_mime("D_WEBVTT/CAPTIONS"), None);
        assert_eq!(mkv_codec_id_to_mime("S_TEXT/UTF8"), None);
    }

    #[test]
    fn test_mkv_codec_id_to_mime_unknown_prefix() {
        assert_eq!(mkv_codec_id_to_mime("X_SOMETHING"), None);
        assert_eq!(mkv_codec_id_to_mime(""), None);
        assert_eq!(mkv_codec_id_to_mime("V"), None);
    }

    // ── Codecs::to_mime_type unit tests ─────────────────────────────────

    #[test]
    fn test_to_mime_type_webm_vp9_opus() {
        let codecs = codecs_from_raw(&["V_VP9", "A_OPUS"]);
        let mime = codecs.to_mime_type(ContainerFormat::WebM);
        assert_eq!(mime, "video/webm; codecs=\"vp9, opus\"");
    }

    #[test]
    fn test_to_mime_type_webm_av1_opus() {
        let codecs = codecs_from_raw(&["V_AV1", "A_OPUS"]);
        let mime = codecs.to_mime_type(ContainerFormat::WebM);
        // Sorted by type priority: V_AV1 (video) before A_OPUS (audio)
        assert_eq!(mime, "video/webm; codecs=\"av01.0.01M.08, opus\"");
    }

    #[test]
    fn test_to_mime_type_mkv() {
        let codecs = codecs_from_raw(&["V_VP9", "A_OPUS"]);
        let mime = codecs.to_mime_type(ContainerFormat::Mkv);
        assert_eq!(mime, "video/x-matroska; codecs=\"vp9, opus\"");
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
        assert_eq!(codecs.get_mkv_codec_ids(), vec!["V_AV1", "A_OPUS", "D_WEBVTT/SUBTITLES"]);
        // Subtitle codecs are filtered out for MIME
        assert_eq!(codecs.get_mime_codec_ids(), vec!["av01.0.01M.08", "opus"]);
        assert_eq!(
            codecs.to_mime_type(ContainerFormat::WebM),
            "video/webm; codecs=\"av01.0.01M.08, opus\""
        );
    }

    #[test]
    fn test_mime_type_from_test_vp9_webm() {
        let codecs = codecs_from_file("test_vp9.webm");

        // test_vp9.webm has: VP9 video, Opus audio, and a WebVTT subtitle track
        assert_eq!(codecs.get_mkv_codec_ids(), vec!["V_VP9", "A_OPUS", "D_WEBVTT/SUBTITLES"]);
        assert_eq!(codecs.get_mime_codec_ids(), vec!["vp9", "opus"]);
        assert_eq!(
            codecs.to_mime_type(ContainerFormat::WebM),
            "video/webm; codecs=\"vp9, opus\""
        );
    }
}
