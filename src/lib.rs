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
pub use remuxer::{RemuxStats, RemuxerState, Remuxer, TrackMapping, remux};
pub use sink::{FileSink, Sink};
pub use source::{SeekType, Source, CutInterval, InputSource};
pub use source_mappings::SourcesMappings;
pub use test_utils::{validate_mkv_output, get_input_duration_ns, MkvValidationReport, ValidationStats};

use mkv_element::prelude::*;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
