mod block_ext;
mod cluster_warpper;
mod error;
mod metling_pot;
mod remuxer;
pub mod sink;
pub mod source;
mod source_mappings;
pub mod test_utils;

// Re-exports
pub use block_ext::ClusterBlockExt;
pub use cluster_warpper::{ClusterReadWrapper, ClusterWriteWrapper, CLUSTER_MAX_DURATION_NS, CLUSTER_MAX_SIZE_BYTES};
pub use error::{Error, Result};
pub use metling_pot::MeltingPot;
pub use mkv_element;
pub use remuxer::{RemuxStats, RemuxerState, RemuxerCutMode, Remuxer, TrackMapping, remux};
pub use sink::{FileSink, Sink};
pub use source::{SeekType, Source, CutInterval, InputSource, Cutting, Remuxing};
pub use source::util::basic_info::MkvBasicInfo;
pub use source_mappings::SourcesMappings;
pub use test_utils::{validate_mkv_output, get_input_duration_ns, MkvValidationReport, ValidationStats};

use mkv_element::prelude::*;

const APP_NAME: &str = env!("CARGO_PKG_NAME");

/// Selects whether the output container should be WebM or Matroska (MKV).
///
/// WebM is a restricted subset of Matroska that only allows specific codecs
/// (VP8, VP9, AV1, Vorbis, Opus, WebVTT) and requires `DocType: webm` in the
/// EBML header. Plain MKV supports any codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerFormat {
    Mkv,
    WebM,
    Vtt
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
            _ => Err(Error::UnsupportedCodec{codec_id: s.to_string(), reason: "must be either 'mkv', 'webm', or 'webvtt'".to_string()}),
        }
    }
}