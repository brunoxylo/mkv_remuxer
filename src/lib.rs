mod block_ext;
mod cluster_warpper;
mod error;
mod metling_pot;
mod remuxer;
pub mod sink;
pub mod source;
mod source_mappings;
mod test_utils;

// Re-exports
pub use block_ext::ClusterBlockExt;
pub use cluster_warpper::{ClusterReadWrapper, ClusterWriteWrapper};
pub use error::{Error, Result};
pub use metling_pot::MeltingPot;
pub use mkv_element;
pub use remuxer::{CutConfig, RemuxStats, TrackMapping, remux};
pub use sink::{FileSink, Sink};
pub use source::{SeekType, Source};
pub use source_mappings::SourcesMappings;

use mkv_element::prelude::*;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
