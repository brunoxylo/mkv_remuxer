

mod error;
pub mod source;
pub mod sink;
mod source_mappings;
mod metling_pot;
mod block_ext;
mod cluster_warpper;
mod remuxer;

// Re-exports
pub use error::{Error, Result};
pub use mkv_element;
pub use source::{Source, SeekType};
pub use sink::{Sink, FileSink};
pub use source_mappings::SourcesMappings;
pub use metling_pot::MeltingPot;
pub use block_ext::{ClusterBlockExt};
pub use cluster_warpper::{ClusterReadWrapper, ClusterWriteWrapper};
pub use remuxer::{remux, CutConfig, TrackMapping, RemuxStats};

use mkv_element::prelude::*;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
