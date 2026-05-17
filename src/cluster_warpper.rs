use crate::block_ext::TrackKind;
use crate::{ClusterBlockExt, Error, Result};
use mkv_element::ClusterBlock;
use mkv_element::prelude::*;

// we limit the cluster size by these bounds
// within these bounds we break at every keyframe
pub const MAX_BLOCKS_PER_CLUSTER: usize = 5000;
pub const MIN_BLOCKS_PER_CLUSTER: usize = 100;

/// this wrapper allows for conveniently iterating over the blocks of a cluster
pub struct ClusterReadWrapper {
    cluster: Cluster,
    /// index of the current block
    block_index: usize,
}

impl ClusterReadWrapper {
    pub fn new(cluster: Cluster) -> Self {
        Self {
            cluster,
            block_index: 0,
        }
    }

    /// Returns a reference to the next block in the cluster without consuming it, or None if there are no more blocks
    pub fn peek_current_block(&self) -> Option<&ClusterBlock> {
        self.cluster.blocks.get(self.block_index)
    }

    /// Advances to the next block in the cluster (consumes the current block)
    pub fn next(&mut self) -> Option<&mut ClusterBlock> {
        self.block_index += 1;
        self.cluster.blocks.get_mut(self.block_index - 1)
    }

    // Step back to the previous block (if possible)
    pub fn step_back(&mut self) {
        if self.block_index > 0 {
            self.block_index -= 1;
        }
    }

    /// Returns the global timestamp of the current block in ns without consuming it, or None if there are no more blocks
    pub fn get_current_absolute_timestamp_ns(&self, timescale: u64) -> Result<i64> {
        let cluster_timestamp = self.cluster.timestamp.0 as i64;
        let block = self
            .cluster
            .blocks
            .get(self.block_index)
            .ok_or_else(|| Error::InvalidBlockData("No more blocks available".to_string()))?;
        block.timestamp_ns(cluster_timestamp, timescale)
    }

    /// Returns true if there are no more blocks to process
    pub fn is_empty(&self) -> bool {
        self.block_index >= self.cluster.blocks.len()
    }
}

/// Wrapper for building a cluster with automatic size and duration tracking
pub struct ClusterWriteWrapper {
    cluster: Cluster,
    /// Duration of the cluster in nanoseconds
    duration_ns: u64,
    /// Size of the cluster in bytes (approximate)
    size_bytes: u64,
    /// Timecode scale for timestamp calculations
    timecode_scale: u64,
}

impl ClusterWriteWrapper {
    /// Create a new cluster with the given starting timestamp in nanoseconds
    pub fn new(start_timestamp_ns: u64, timecode_scale: u64) -> Self {
        // Convert nanoseconds to ticks
        let start_timestamp_ticks = start_timestamp_ns / timecode_scale;

        Self {
            cluster: Cluster {
                timestamp: Timestamp(start_timestamp_ticks),
                blocks: Vec::new(),
                crc32: None,
                void: None,
                position: None,
                prev_size: None,
            },
            duration_ns: 0,
            size_bytes: 0,
            timecode_scale,
        }
    }

    /// Add a block to the cluster with its absolute timestamp in nanoseconds
    /// The relative timestamp will be calculated automatically
    pub fn add_block(
        &mut self,
        block: &ClusterBlock,
        absolute_timestamp_ns: u64,
        track_number: Option<u64>,
        track_kind: Option<TrackKind>,
    ) -> Result<()> {
        // Estimate block size (header + data)
        let block_size = match &block {
            ClusterBlock::Simple(sb) => sb.0.len(),
            ClusterBlock::Group(bg) => bg.block.0.len(),
        };
        // Update duration (last block timestamp - first block timestamp)
        let block_end_ns = absolute_timestamp_ns;
        let cluster_start_ns = self.cluster.timestamp.0 * self.timecode_scale;
        let cluster_duration = (block_end_ns as i64 - cluster_start_ns as i64).max(0);

        let is_video_keyframe = match track_kind {
            Some(TrackKind::Video) => block.is_keyframe().unwrap_or(false),
            _ => false,
        };
        let current_blocks = self.cluster.blocks.len();

        // - break at every keyframe if we have at least MIN_BLOCKS_PER_CLUSTER blocks
        // - strictly limit to MAX_BLOCKS_PER_CLUSTER blocks
        if current_blocks >= MAX_BLOCKS_PER_CLUSTER
            || (current_blocks >= MIN_BLOCKS_PER_CLUSTER && is_video_keyframe)
        {
            return Err(Error::ClusterIsFull(format!(
                "Triggered cluster split. Blocks: {}, Keyframe: {}",
                current_blocks, is_video_keyframe
            )));
        }

        self.duration_ns = cluster_duration as u64;
        self.size_bytes += block_size as u64;

        // set timestamp of the block
        let mut new_block = block.clone();
        new_block.set_timestamp_ns(
            absolute_timestamp_ns.max(0) as u64,
            self.cluster.timestamp.0,
            self.timecode_scale,
        )?;
        if let Some(track_number) = track_number {
            new_block.set_track_number(track_number)?;
        }
        let mut insert_index = self.cluster.blocks.len();

        // if we have a video keyframe and there are blocks before from other tracks
        // with the same timestamp we want to insert this video block before them
        // this will ensure that every cluster starts with a video keyframe
        // even if audio tracks occur before with the same timestamp
        // (Firefox MSE requires that)
        if is_video_keyframe {
            let new_raw_ts = new_block.timestamp()?;
            for (i, block) in self.cluster.blocks.iter().enumerate().rev() {
                // walk back until we see a block with a differnt timestamp
                // or with the same track number (we never want to change the block order within tracks)
                if block.timestamp()? == new_raw_ts
                    && block.track_number()? != new_block.track_number()?
                {
                    insert_index = i;
                } else {
                    break;
                }
            }
        }
        self.cluster.blocks.insert(insert_index, new_block);

        Ok(())
    }

    /// Get the current duration in nanoseconds
    pub fn duration_ns(&self) -> u64 {
        self.duration_ns
    }

    /// Get the current size in bytes
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// Get the number of blocks in the cluster
    pub fn block_count(&self) -> usize {
        self.cluster.blocks.len()
    }

    /// Check if the cluster is empty
    pub fn is_empty(&self) -> bool {
        self.cluster.blocks.is_empty()
    }

    /// Consume the wrapper and return the completed cluster
    pub fn finish(self) -> Cluster {
        self.cluster
    }

    /// Get a reference to the cluster without consuming it
    pub fn cluster(&self) -> &Cluster {
        &self.cluster
    }
}
