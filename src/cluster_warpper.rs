use crate::{ClusterBlockExt, Error, Result};
use mkv_element::ClusterBlock;
use mkv_element::prelude::*;

pub const CLUSTER_MAX_DURATION_NS: u64 = 5_000_000_000; // 5 seconds in nanoseconds
pub const CLUSTER_MAX_SIZE_BYTES: u64 = 10_000_000; // 10 MB

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
    cluster_max_duration_ns: u64,
    cluster_max_size_bytes: u64,
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
            cluster_max_duration_ns: CLUSTER_MAX_DURATION_NS,
            cluster_max_size_bytes: CLUSTER_MAX_SIZE_BYTES,
        }
    }

    /// Overwrite the default limits for the cluster
    pub fn overwrite_limits(&mut self, max_duration_ns: u64, max_size_bytes: u64) {
        self.cluster_max_duration_ns = max_duration_ns;
        self.cluster_max_size_bytes = max_size_bytes;
    }

    /// Add a block to the cluster with its absolute timestamp in nanoseconds
    /// The relative timestamp will be calculated automatically
    pub fn add_block(&mut self, block: &ClusterBlock, absolute_timestamp_ns: i64, track_number: Option<u64>) -> Result<()> {
        // Estimate block size (header + data)
        let block_size = match &block {
            ClusterBlock::Simple(sb) => sb.0.len(),
            ClusterBlock::Group(bg) => bg.block.0.len(),
        };
        // Update duration (last block timestamp - first block timestamp)
        let block_end_ns = absolute_timestamp_ns;
        let cluster_start_ns = self.cluster.timestamp.0 * self.timecode_scale;
        let cluster_duration = (block_end_ns as i64 - cluster_start_ns as i64).max(0);

        // Check if the cluster has reached size or duration limits
        if cluster_duration > self.cluster_max_duration_ns as i64
            || self.size_bytes as i64 + block_size as i64 > self.cluster_max_size_bytes as i64
        {
            return Err(Error::ClusterIsFull(format!(
                "limit bytes: {}, duration: {}, block bytes: {}",
                self.cluster_max_size_bytes, self.cluster_max_duration_ns, block_size
            )));
        }
        self.duration_ns = cluster_duration as u64;
        self.size_bytes += block_size as u64;

        // Add block to cluster
        self.cluster.blocks.push(block.clone());
        // get last one as mutable reference
        let target_block = self
            .cluster
            .blocks
            .last_mut()
            .ok_or_else(|| Error::InvalidBlockData("Failed to get last block".to_string()))?;
        target_block.set_timestamp_ns(
            absolute_timestamp_ns.max(0) as u64,
            self.cluster.timestamp.0,
            self.timecode_scale,
        )?;
        if let Some(track_number) = track_number {
            target_block.set_track_number(track_number)?;
        }

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
