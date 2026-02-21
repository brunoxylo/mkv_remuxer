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
    /// Timestamp of first block in nanoseconds (None if no blocks yet)
    first_block_ns: Option<i64>,
    /// Timestamp of last block in nanoseconds (None if no blocks yet)
    last_block_ns: Option<i64>,
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
            first_block_ns: None,
            last_block_ns: None,
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
    pub fn add_block(&mut self, block: &ClusterBlock, absolute_timestamp_ns: i64) -> Result<()> {
        // Estimate block size (header + data)
        let block_size = match &block {
            ClusterBlock::Simple(sb) => sb.0.len(),
            ClusterBlock::Group(bg) => bg.block.0.len(),
        };
        
        // Calculate what the cluster duration would be if we add this block
        let new_duration_ns = if let Some(first) = self.first_block_ns {
            // Cluster already has blocks, calculate span including this new block
            let earliest = first.min(absolute_timestamp_ns);
            let latest = self.last_block_ns.unwrap().max(absolute_timestamp_ns);
            (latest - earliest).max(0) as u64
        } else {
            // This would be the first block, duration is 0
            0
        };

        // Check if adding this block would exceed limits
        if new_duration_ns > self.cluster_max_duration_ns
            || self.size_bytes + block_size as u64 > self.cluster_max_size_bytes
        {
            return Err(Error::ClusterIsFull(format!(
                "limit bytes: {}, duration: {} ns, new duration: {} ns, block bytes: {}",
                self.cluster_max_size_bytes, self.cluster_max_duration_ns, new_duration_ns, block_size
            )));
        }
        
        // Limits OK, now update tracking
        if self.first_block_ns.is_none() {
            self.first_block_ns = Some(absolute_timestamp_ns);
        }
        self.last_block_ns = Some(absolute_timestamp_ns);
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
            absolute_timestamp_ns,
            self.cluster.timestamp.0 as i64,
            self.timecode_scale,
        )?;

        Ok(())
    }

    /// Get the current duration in nanoseconds
    pub fn duration_ns(&self) -> u64 {
        if let (Some(first), Some(last)) = (self.first_block_ns, self.last_block_ns) {
            (last - first).max(0) as u64
        } else {
            0
        }
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
