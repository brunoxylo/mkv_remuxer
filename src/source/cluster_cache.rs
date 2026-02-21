use crate::{Error, Result};
use mkv_element::io::blocking_impl::*;
use mkv_element::{ClusterBlock, prelude::*};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use crate::block_ext::ClusterBlockExt;
use crate::codec_parsers::{Vp8FrameHeader, Vp9FrameHeader, Av1FrameHeader};

/// Calculates pre-roll frames needed for frame-accurate video cutting.
///
/// For VP8/VP9/AV1: Parses frame headers and tracks reference slot dependencies
/// to determine minimum set of frames needed for decoder state.
///
/// For other codecs (H.264/HEVC/etc): Uses simple keyframe snapping - includes
/// all frames since the last keyframe before the cut point.
///
/// # Important Notes
/// - MKV lacing: If a laced block contains needed frames, the entire block must be kept
/// - Performance: Frame header parsing is done on-demand, may be slow for long sequences
/// - Scans backwards from cut point on-demand (may re-scan on repeated calls)
pub struct PreRollCalculator {
    file: File,
    timecode_scale: u64,
    codec_type: CodecType,
}

/// Codec type determines pre-roll calculation strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodecType {
    VP8,
    VP9,
    AV1,
    /// For H.264, HEVC, and unknown codecs: use simple keyframe-based strategy
    Other,
}

/// Information about a block and its cluster context
#[derive(Debug, Clone)]
struct BlockInfo {
    block: ClusterBlock,
    cluster_timestamp: u64, // Cluster timestamp in timecode units
    file_position: u64,      // Position of cluster in file
}

/// Reference slot tracker for VP8/VP9/AV1 codecs
/// Tracks which reference frame slots are needed and which have been satisfied
struct ReferenceSlotTracker {
    /// Number of slots (3 for VP8, 8 for VP9/AV1)
    num_slots: usize,
    /// Which slots are needed (true = need data for this slot)
    needed: Vec<bool>,
    /// Which slots have been satisfied (true = we have a frame that writes to this slot)
    satisfied: Vec<bool>,
}

impl ReferenceSlotTracker {
    fn new(num_slots: usize) -> Self {
        Self {
            num_slots,
            needed: vec![false; num_slots],
            satisfied: vec![false; num_slots],
        }
    }

    /// Mark slots as needed (frame reads from these slots)
    fn mark_needed(&mut self, slots: &[u8]) {
        for &slot in slots {
            if (slot as usize) < self.num_slots {
                self.needed[slot as usize] = true;
            }
        }
    }

    /// Mark slots as satisfied (frame writes to these slots)
    fn mark_satisfied(&mut self, slots: &[u8]) {
        for &slot in slots {
            if (slot as usize) < self.num_slots {
                self.satisfied[slot as usize] = true;
            }
        }
    }

    /// Check if all needed slots are satisfied
    fn all_satisfied(&self) -> bool {
        for i in 0..self.num_slots {
            if self.needed[i] && !self.satisfied[i] {
                return false;
            }
        }
        true
    }

    /// Reset tracker for a new analysis
    fn reset(&mut self) {
        self.needed.fill(false);
        self.satisfied.fill(false);
    }
}

impl CodecType {
    /// Detect codec type from MKV codec ID string
    fn from_codec_id(codec_id: &str) -> Self {
        match codec_id {
            "V_VP8" => CodecType::VP8,
            "V_VP9" => CodecType::VP9,
            "V_AV1" => CodecType::AV1,
            _ => CodecType::Other, // H.264 (V_MPEG4/ISO/AVC), HEVC (V_MPEGH/ISO/HEVC), etc.
        }
    }
}

impl PreRollCalculator {
    /// Create a new pre-roll calculator
    ///
    /// # Arguments
    /// * `file` - File handle for reading MKV data
    /// * `timecode_scale` - MKV timecode scale from Segment Info
    /// * `codec_id` - MKV codec ID string (e.g., "V_VP9", "V_AV1", "V_MPEG4/ISO/AVC")
    pub fn new(file: File, timecode_scale: u64, codec_id: &str) -> Self {
        Self {
            file,
            timecode_scale,
            codec_type: CodecType::from_codec_id(codec_id),
        }
    }

    /// Get all blocks that should be included before the cut point for pre-roll
    ///
    /// Returns blocks in chronological order (oldest first) that are needed
    /// to properly decode the frame at cut_timestamp_ns.
    ///
    /// # Arguments
    /// * `cut_timestamp_ns` - The timestamp where cutting occurs (nanoseconds)
    /// * `track_num` - Track number to analyze
    /// * `start_scan_position` - File position to start scanning backwards from (usually near cut point)
    ///
    /// # Returns
    /// Vector of (ClusterBlock, cluster_timestamp_ns) tuples in chronological order
    ///
    /// # Design Notes
    /// - For VP8/VP9/AV1: Parses frame headers to track reference dependencies (TODO)
    /// - For other codecs: Returns all frames from last keyframe to cut point
    /// - Laced blocks: Entire block is kept if any frame within is needed
    /// - Scans backwards on-demand, may be slow for cuts far from keyframes
    pub fn get_previous_blocks_to_keep(
        &mut self,
        cut_timestamp_ns: i64,
        track_num: u64,
        start_scan_position: u64,
    ) -> Result<Vec<(ClusterBlock, i64)>> { // Returns (ClusterBlock, cluster_timestamp_ns)
        match self.codec_type {
            CodecType::VP8 | CodecType::VP9 | CodecType::AV1 => {
                // TODO: Implement codec-aware reference tracking
                // For now, fall back to keyframe strategy
                self.get_preroll_simple_keyframe(cut_timestamp_ns, track_num, start_scan_position)
            }
            CodecType::Other => {
                self.get_preroll_simple_keyframe(cut_timestamp_ns, track_num, start_scan_position)
            }
        }
    }

    /// Scan backwards from a file position to collect clusters until we go too far back
    ///
    /// Returns clusters in reverse chronological order (newest first)
    fn scan_clusters_backwards(
        &mut self,
        start_position: u64,
        cut_timestamp_ns: i64,
    ) -> Result<Vec<(u64, Cluster)>> {
        let mut clusters = Vec::new();
        const MAX_LOOKBACK_NS: i64 = 30_000_000_000; // 30 seconds max lookback
        const MAX_CLUSTERS: usize = 100; // Safety limit

        // Seek to start position
        self.file.seek(SeekFrom::Start(start_position))?;

        // Scan backwards by repeatedly seeking to previous elements
        // This is inefficient but works without a full file index
        let mut current_pos = start_position;
        
        loop {
            if clusters.len() >= MAX_CLUSTERS {
                break;
            }

            // Try to find a cluster before current position
            match self.find_previous_cluster(current_pos)? {
                Some((cluster_pos, cluster)) => {
                    let cluster_ts_ns = (cluster.timestamp.0 * self.timecode_scale) as i64;
                    
                    // Stop if we've gone too far back in time
                    if cut_timestamp_ns - cluster_ts_ns > MAX_LOOKBACK_NS {
                        break;
                    }

                    clusters.push((cluster_pos, cluster));
                    current_pos = cluster_pos;
                }
                None => break, // No more clusters found
            }
        }

        Ok(clusters)
    }

    /// Find the cluster immediately before the given file position
    ///
    /// Scans backwards from position looking for Cluster elements
    /// Returns None if no cluster found or reached beginning of file
    fn find_previous_cluster(&mut self, from_position: u64) -> Result<Option<(u64, Cluster)>> {
        if from_position == 0 {
            return Ok(None);
        }

        // Scan backwards in chunks looking for cluster headers
        const SCAN_CHUNK_SIZE: u64 = 64 * 1024; // 64KB chunks
        const MIN_POSITION: u64 = 1024; // Don't scan before segment start
        
        let mut scan_end = from_position.saturating_sub(1);
        
        while scan_end > MIN_POSITION {
            let scan_start = scan_end.saturating_sub(SCAN_CHUNK_SIZE).max(MIN_POSITION);
            
            // Try each position in the range
            for pos in (scan_start..scan_end).rev() {
                self.file.seek(SeekFrom::Start(pos))?;
                
                if let Ok(header) = Header::read_from(&mut self.file) {
                    if header.id == Cluster::ID {
                        // Found a cluster header, read it
                        if let Ok(cluster) = Cluster::read_element(&header, &mut self.file) {
                            return Ok(Some((pos, cluster)));
                        }
                    }
                }
            }
            
            scan_end = scan_start;
            
            // Safety: stop if we've scanned too much without finding anything
            if from_position - scan_end > 10 * 1024 * 1024 { // 10MB limit
                return Ok(None);
            }
        }

        Ok(None)
    }

    /// Simple keyframe-based pre-roll: include all frames from last keyframe to cut point
    ///
    /// This is the "squeeze" logic - works for H.264/HEVC/etc where cutting at
    /// keyframes is safe.
    fn get_preroll_simple_keyframe(
        &mut self,
        cut_timestamp_ns: i64,
        track_num: u64,
        start_scan_position: u64,
    ) -> Result<Vec<(ClusterBlock, i64)>> {
        let mut blocks_to_keep = Vec::new();
        let mut found_keyframe = false;

        // Scan backwards from start position to find clusters
        let clusters = self.scan_clusters_backwards(start_scan_position, cut_timestamp_ns)?;

        // Walk through clusters in reverse (oldest to newest)
        for (cluster_pos, cluster) in clusters.into_iter().rev() {
            let cluster_ts_ns = (cluster.timestamp.0 * self.timecode_scale) as i64;

            // Process blocks in chronological order
            for block in &cluster.blocks {
                // Get track number (handle Result)
                let block_track_num = match block.track_number() {
                    Ok(tn) => tn,
                    Err(_) => continue, // Skip blocks with invalid track number
                };

                if block_track_num != track_num {
                    continue;
                }

                let block_ts_ns = block.timestamp_ns(cluster.timestamp.0 as i64, self.timecode_scale)?;
                
                // Skip blocks at or after cut point
                if block_ts_ns >= cut_timestamp_ns {
                    continue;
                }

                // Check if this is a keyframe (handle Result)
                let is_keyframe = match block.is_keyframe() {
                    Ok(kf) => kf,
                    Err(_) => false, // Assume non-keyframe if we can't determine
                };
                
                if is_keyframe {
                    // Found keyframe, start collecting from here
                    found_keyframe = true;
                    blocks_to_keep.clear(); // Discard any previous non-keyframe blocks
                }

                // Collect block if we're after a keyframe
                if found_keyframe {
                    blocks_to_keep.push((block.clone(), cluster_ts_ns));
                }
            }

            // If we found a keyframe, we can stop scanning
            if found_keyframe {
                break;
            }
        }

        if !found_keyframe {
            return Err(Error::InvalidConfig(
                "No keyframe found before cut point within scan window".to_string()
            ));
        }

        Ok(blocks_to_keep)
    }

    /// Codec-aware pre-roll for VP8/VP9/AV1: track reference slot dependencies
    ///
    /// This method intelligently determines the minimal set of pre-roll frames needed
    /// by tracking reference buffer slots. Unlike simple keyframe strategies, this
    /// approach understands codec semantics and can minimize pre-roll overhead.
    fn get_preroll_codec_aware(
        &mut self,
        cut_timestamp_ns: i64,
        track_num: u64,
        start_scan_position: u64,
    ) -> Result<Vec<(ClusterBlock, i64)>> {
        use crate::codec_parsers::{vp8_parser::Vp8FrameHeader, vp9_parser::Vp9FrameHeader, av1_parser::Av1FrameHeader};

        // Determine number of slots based on codec
        let num_slots = match self.codec_type {
            CodecType::VP8 => 3,
            CodecType::VP9 | CodecType::AV1 => 8,
            CodecType::Other => {
                // Fallback to keyframe strategy for unknown codecs
                return self.get_preroll_simple_keyframe(cut_timestamp_ns, track_num, start_scan_position);
            }
        };

        let mut tracker = ReferenceSlotTracker::new(num_slots);
        let mut blocks_to_keep: Vec<(ClusterBlock, i64)> = Vec::new();

        // Scan backwards through clusters
        let clusters = self.scan_clusters_backwards(start_scan_position, cut_timestamp_ns)?;

        // Process blocks in reverse chronological order (backward scan)
        for (_cluster_pos, cluster) in clusters.iter() {
            let cluster_ts_ns = (cluster.timestamp.0 * self.timecode_scale) as i64;

            // Process blocks for this cluster
            for block in &cluster.blocks {
                // Check track number
                let block_track_num = match block.track_number() {
                    Ok(tn) => tn,
                    Err(_) => continue,
                };
                
                if block_track_num != track_num {
                    continue;
                }

                // Calculate absolute timestamp
                let block_ts_offset = match block.timestamp() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let timestamp_ns = cluster_ts_ns + (block_ts_offset as i64 * self.timecode_scale as i64);

                // Only consider blocks before the cut point
                if timestamp_ns >= cut_timestamp_ns {
                    continue;
                }

                // If we hit a keyframe, we can stop - it refreshes all slots
                if block.is_keyframe().unwrap_or(false) {
                    blocks_to_keep.push((block.clone(), timestamp_ns));
                    break;
                }

                // Extract frame data from the block
                let frame_data = match self.extract_frame_data(block) {
                    Ok(data) => data,
                    Err(e) => {
                        eprintln!("Warning: Failed to extract frame data: {}. Falling back to keyframe strategy.", e);
                        return self.get_preroll_simple_keyframe(cut_timestamp_ns, track_num, start_scan_position);
                    }
                };

                // Parse frame header based on codec type
                let parse_result: Result<(Vec<usize>, Vec<usize>)> = match self.codec_type {
                    CodecType::VP8 => {
                        match Vp8FrameHeader::parse(&frame_data) {
                            Ok(header) => {
                                // Convert Vec<u8> to Vec<usize>
                                let deps: Vec<usize> = header.get_dependency_slots().iter().map(|&x| x as usize).collect();
                                let updates: Vec<usize> = header.get_updated_slots().iter().map(|&x| x as usize).collect();
                                Ok((deps, updates))
                            }
                            Err(e) => Err(Error::RemuxError(format!("VP8 parse error: {}", e))),
                        }
                    }
                    CodecType::VP9 => {
                        match Vp9FrameHeader::parse(&frame_data) {
                            Ok(header) => {
                                // Convert &[u8; 3] and Vec<u8> to Vec<usize>
                                let deps: Vec<usize> = header.get_dependency_slots().iter().map(|&x| x as usize).collect();
                                let updates: Vec<usize> = header.get_updated_slots().iter().map(|&x| x as usize).collect();
                                Ok((deps, updates))
                            }
                            Err(e) => Err(Error::RemuxError(format!("VP9 parse error: {}", e))),
                        }
                    }
                    CodecType::AV1 => {
                        match Av1FrameHeader::parse(&frame_data) {
                            Ok(header) => {
                                // Convert &[u8; 7] and Vec<u8> to Vec<usize>
                                let deps: Vec<usize> = header.get_dependency_slots().iter().map(|&x| x as usize).collect();
                                let updates: Vec<usize> = header.get_updated_slots().iter().map(|&x| x as usize).collect();
                                Ok((deps, updates))
                            }
                            Err(e) => Err(Error::RemuxError(format!("AV1 parse error: {}", e))),
                        }
                    }
                    CodecType::Other => unreachable!(), // Already handled above
                };

                match parse_result {
                    Ok((dependency_slots, updated_slots)) => {
                        // Check if this frame satisfies any needed slots
                        let mut satisfies_need = false;
                        
                        // Convert Vec<usize> to Vec<u8> for mark_satisfied
                        let updated_u8: Vec<u8> = updated_slots.iter().map(|&x| x as u8).collect();
                        for &slot in &updated_slots {
                            if slot < num_slots && tracker.needed[slot] && !tracker.satisfied[slot] {
                                satisfies_need = true;
                            }
                        }
                        
                        if satisfies_need {
                            tracker.mark_satisfied(&updated_u8);
                        }

                        // If this frame satisfies a need, include it
                        if satisfies_need {
                            // Mark the dependencies of this frame as needed
                            let dependency_u8: Vec<u8> = dependency_slots.iter().map(|&x| x as u8).collect();
                            tracker.mark_needed(&dependency_u8);

                            blocks_to_keep.push((block.clone(), timestamp_ns));
                        }

                        // Check if all needed slots are satisfied
                        if tracker.all_satisfied() {
                            break;
                        }
                    }
                    Err(e) => {
                        // Parse failed, fall back to keyframe strategy
                        eprintln!("Warning: Frame header parse failed: {}. Falling back to keyframe strategy.", e);
                        return self.get_preroll_simple_keyframe(cut_timestamp_ns, track_num, start_scan_position);
                    }
                }
            }

            // If we found a keyframe or satisfied all slots, stop
            if !blocks_to_keep.is_empty() {
                let last_block = &blocks_to_keep[blocks_to_keep.len() - 1].0;
                if last_block.is_keyframe().unwrap_or(false) || tracker.all_satisfied() {
                    break;
                }
            }
        }

        // Reverse to chronological order
        blocks_to_keep.reverse();

        if blocks_to_keep.is_empty() {
            return Err(Error::InvalidBlockData(
                "No suitable pre-roll frames found within scan window".to_string()
            ));
        }

        Ok(blocks_to_keep)
    }

    /// Extract raw frame data from MKV block
    ///
    /// Handles block structure parsing but currently only supports non-laced frames.
    /// For laced frames (multiple frames in one block), returns an error which triggers
    /// fallback to keyframe strategy.
    fn extract_frame_data(&self, block: &ClusterBlock) -> Result<Vec<u8>> {
        use crate::block_ext::ClusterBlockExt;

        // Get block data via the extension trait
        let data = block.get_data()?;

        // Parse track number VInt
        let track_vint_len = vint_length(data[0]);
        if data.len() < track_vint_len + 3 {
            return Err(Error::InvalidBlockData(
                "Block data too short".to_string()
            ));
        }

        // Skip track number + timestamp (2 bytes) + flags (1 byte)
        let header_len = track_vint_len + 2 + 1;
        let flags = data[track_vint_len + 2];

        // Check lacing flags (bits 1-2)
        let lacing = (flags >> 1) & 0x03;
        if lacing != 0 {
            // Laced frame - not yet supported, fall back to keyframe strategy
            return Err(Error::UnsupportedOperation(
                format!("Laced frames not yet supported (lacing type: {})", lacing)
            ));
        }

        // Extract frame data (everything after header)
        if data.len() <= header_len {
            return Err(Error::InvalidBlockData(
                "Block has no frame data after header".to_string()
            ));
        }

        Ok(data[header_len..].to_vec())
    }
}

/// Determine the length of a variable-length integer (VInt) from its first byte
///
/// EBML/Matroska uses VInt encoding where the number of leading zero bits
/// indicates the total length:
/// - 1xxx xxxx: 1 byte
/// - 01xx xxxx xxxx xxxx: 2 bytes
/// - 001x xxxx ...: 3 bytes
/// - etc.
fn vint_length(byte: u8) -> usize {
    if byte & 0x80 != 0 {
        1
    } else if byte & 0x40 != 0 {
        2
    } else if byte & 0x20 != 0 {
        3
    } else if byte & 0x10 != 0 {
        4
    } else if byte & 0x08 != 0 {
        5
    } else if byte & 0x04 != 0 {
        6
    } else if byte & 0x02 != 0 {
        7
    } else {
        8
    }
}
