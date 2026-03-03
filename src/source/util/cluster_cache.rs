use crate::{Error, Result};
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use crate::block_ext::{ClusterBlockExt, ClusterExt};

/// Lightweight cache for keyframe positions in the current cluster of interest
/// to speed up freeze seek operations without fully parsing all blocks in the cluster.
///
/// Created via binary search to find the cluster containing a given timestamp,
/// then lazily caches keyframe positions when queried.
pub struct KeyframePositionCache {
    pub position: u64,
    file: File,
    timecode_scale: u64,
    reference_timestamp_ns: i64,
    keyframe_timestamp_ns: HashMap<(u64, bool), i64>, // (track_num, after or before) -> timestamp_ns of keyframe
    keyframe_cluster_position: HashMap<(u64, bool), u64>, // (track_num, after or before) -> file position of the cluster that holds the keyframe
}

impl KeyframePositionCache {
    /// Create a new cache by binary-searching for the cluster containing `timestamp_ns`.
    ///
    /// * `file` — file handle (will be seeked during search)
    /// * `timecode_scale` — the MKV timecode scale in nanoseconds
    /// * `timestamp_ns` — the reference timestamp to search for (stored for later keyframe queries)
    /// * `range` — optional `(lo, hi)` byte range to confine the search;
    ///   when `None`, the entire file is searched.
    pub fn new(
        mut file: File,
        timecode_scale: u64,
        timestamp_ns: i64,
        range: Option<(u64, u64)>,
    ) -> Result<Self> {
        let (lo, hi) = match range {
            Some((start, end)) => (start, end),
            None => {
                let file_len = file.metadata()?.len();
                (0, file_len)
            }
        };

        let position = Self::binary_search_cluster(&mut file, timecode_scale, timestamp_ns, lo, hi)?;

        Ok(Self {
            position,
            file,
            timecode_scale,
            reference_timestamp_ns: timestamp_ns,
            keyframe_timestamp_ns: HashMap::new(),
            keyframe_cluster_position: HashMap::new(),
        })
    }

    /// Returns the reference timestamp (in nanoseconds) that this cache was
    /// constructed with.
    pub fn get_timestamp_ns(&self) -> i64 {
        self.reference_timestamp_ns
    }

    pub fn get_keyframe_timestamp_ns(
        &mut self,
        track_num: u64,
        after: bool,
    ) -> Result<i64> {
        if let Some(ts) =
            self.keyframe_timestamp_ns
                .get(&(track_num, after))
        {
            Ok(*ts)
        } else {
            self.update_cache(track_num, after)?;
            let ts = self
                .keyframe_timestamp_ns
                .get(&(track_num, after))
                .ok_or(Error::InvalidConfig(
                    "Keyframe timestamp not found".to_string(),
                ))?;
            Ok(*ts)
        }
    }

    pub fn get_closest_keyframe_timestamp_ns(
        &mut self,
        track_num: u64,
    ) -> Result<i64> {
        let after_ts = self.get_keyframe_timestamp_ns(track_num, true)?;
        let before_ts = self.get_keyframe_timestamp_ns(track_num, false)?;

        let after_diff = (after_ts - self.reference_timestamp_ns).abs();
        let before_diff = (before_ts - self.reference_timestamp_ns).abs();

        if after_diff < before_diff {
            Ok(after_ts)
        } else {
            Ok(before_ts)
        }
    }

    /// Returns the file position of the cluster that contains the keyframe
    /// nearest to the reference timestamp for the given `track_num` and direction.
    pub fn get_keyframe_cluster_position(
        &mut self,
        track_num: u64,
        after: bool,
    ) -> Result<u64> {
        if let Some(pos) =
            self.keyframe_cluster_position
                .get(&(track_num, after))
        {
            Ok(*pos)
        } else {
            self.update_cache(track_num, after)?;
            let pos = self
                .keyframe_cluster_position
                .get(&(track_num, after))
                .ok_or(Error::InvalidConfig(
                    "Keyframe cluster position not found".to_string(),
                ))?;
            Ok(*pos)
        }
    }

    // ── Binary-search helpers (static, operate on a bare File) ──────────

    /// Binary-search the file for the cluster whose timestamp is closest to
    /// (but ≤) `target_timestamp_ns`. Returns the file position of that cluster.
    fn binary_search_cluster(
        file: &mut File,
        timecode_scale: u64,
        target_timestamp_ns: i64,
        mut lo: u64,
        mut hi: u64,
    ) -> Result<u64> {
        let target_unsigned = target_timestamp_ns.max(0) as u64;

        // Phase 1: Binary search on file byte positions.
        // Each iteration reads ~10 bytes (scan for 4-byte ID + read header + timestamp)
        // instead of parsing megabytes of block data per cluster.
        while hi.saturating_sub(lo) > 1_048_576 {
            let mid = lo + (hi - lo) / 2;
            match Self::scan_to_next_cluster_in(file, mid, hi)? {
                Some(cluster_pos) => {
                    match Self::read_cluster_timestamp_at(file, cluster_pos, timecode_scale) {
                        Ok((ts, _)) => {
                            if ts <= target_unsigned {
                                lo = cluster_pos;
                            } else {
                                hi = cluster_pos;
                            }
                        }
                        Err(_) => {
                            // Invalid cluster (false positive from byte scan), narrow from above
                            hi = cluster_pos;
                        }
                    }
                }
                None => {
                    // No cluster between mid and hi
                    hi = mid;
                }
            }
        }

        // Phase 2: Sequential scan of the remaining ≤1 MB range.
        // Reads element headers and Cluster timestamps only — block data is skipped.
        let cluster_index = Self::collect_clusters_in(file, timecode_scale, lo, hi)?;

        if cluster_index.is_empty() {
            return Err(Error::InvalidConfig("No clusters found".to_string()));
        }

        // Find the best cluster for the target timestamp
        let result = cluster_index.binary_search_by_key(&target_unsigned, |(_, ts)| *ts);

        let cluster_idx = match result {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        };

        Ok(cluster_index[cluster_idx].0)
    }

    /// Read only the Cluster timestamp at a given file position without parsing
    /// any block data. Reads ~10-20 bytes instead of potentially megabytes.
    /// Returns (timestamp_ns, position_after_this_cluster).
    fn read_cluster_timestamp_at(
        file: &mut File,
        pos: u64,
        timecode_scale: u64,
    ) -> Result<(u64, u64)> {
        file.seek(SeekFrom::Start(pos))?;
        let header = Header::read_from(file)?;
        if header.id != Cluster::ID {
            return Err(Error::InvalidConfig(format!(
                "Expected Cluster at position {}, found ID: {:x}",
                pos, header.id.value
            )));
        }

        let body_start = file.stream_position()?;
        let next_element_pos = if header.size.is_unknown {
            None
        } else {
            Some(body_start + header.size.value)
        };

        // The Timestamp child (ID 0xE7) is specified to be the first or second
        // child element of a Cluster. Scan at most 5 children to be safe.
        for _ in 0..5 {
            if let Some(end) = next_element_pos {
                if file.stream_position()? >= end {
                    break;
                }
            }
            let child_header = match Header::read_from(file) {
                Ok(h) => h,
                Err(_) => break,
            };
            if child_header.id == Timestamp::ID {
                let ts = Timestamp::read_element(&child_header, file)?;
                let timestamp_ns = ts.0 * timecode_scale;
                let after = next_element_pos.unwrap_or_else(|| file.stream_position().unwrap_or(pos));
                return Ok((timestamp_ns, after));
            }
            // Skip this child
            if child_header.size.value > 0 && !child_header.size.is_unknown {
                file.seek(SeekFrom::Current(child_header.size.value as i64))?;
            } else {
                break;
            }
        }

        // Timestamp not found (shouldn't happen in valid MKV), default to 0
        let after = next_element_pos.unwrap_or(file.stream_position().unwrap_or(pos));
        Ok((0, after))
    }

    /// Scan forward from `from` looking for the 4-byte Cluster EBML ID pattern
    /// (0x1F 0x43 0xB6 0x75). Returns the file position of the Cluster header,
    /// or `None` if not found before `limit`.
    fn scan_to_next_cluster_in(
        file: &mut File,
        from: u64,
        limit: u64,
    ) -> Result<Option<u64>> {
        file.seek(SeekFrom::Start(from))?;

        const CLUSTER_ID: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
        const BUF_SIZE: usize = 8192;
        let mut buf = [0u8; BUF_SIZE];
        let mut file_pos = from;
        let mut matched = 0usize;

        while file_pos < limit {
            let to_read = ((limit - file_pos) as usize).min(BUF_SIZE);
            let n = file.read(&mut buf[..to_read])?;
            if n == 0 {
                break;
            }

            for i in 0..n {
                if buf[i] == CLUSTER_ID[matched] {
                    matched += 1;
                    if matched == 4 {
                        // Pattern matched; cluster header starts 3 bytes before current
                        let cluster_pos = file_pos + i as u64 - 3;
                        return Ok(Some(cluster_pos));
                    }
                } else if buf[i] == CLUSTER_ID[0] {
                    matched = 1;
                } else {
                    matched = 0;
                }
            }

            file_pos += n as u64;
        }

        Ok(None)
    }

    /// Sequentially scan from a known element boundary, reading only headers and
    /// Cluster timestamps, skipping all block data. Returns a small Vec of
    /// (file_position, timestamp_ns) for clusters in the range [from, limit).
    fn collect_clusters_in(
        file: &mut File,
        timecode_scale: u64,
        from: u64,
        limit: u64,
    ) -> Result<Vec<(u64, u64)>> {
        let mut clusters = Vec::new();
        let mut pos = from;

        loop {
            if pos >= limit {
                break;
            }
            file.seek(SeekFrom::Start(pos))?;

            let header = match Header::read_from(file) {
                Ok(h) => h,
                Err(_) => break,
            };

            let body_start = file.stream_position()?;

            if header.id == Cluster::ID {
                // Read only the Timestamp child, skip everything else
                let mut timestamp_ticks = 0u64;
                let body_end = if header.size.is_unknown {
                    None
                } else {
                    Some(body_start + header.size.value)
                };

                for _ in 0..5 {
                    if let Some(end) = body_end {
                        if file.stream_position()? >= end {
                            break;
                        }
                    }
                    let child_h = match Header::read_from(file) {
                        Ok(h) => h,
                        Err(_) => break,
                    };
                    if child_h.id == Timestamp::ID {
                        if let Ok(ts) = Timestamp::read_element(&child_h, file) {
                            timestamp_ticks = ts.0;
                        }
                        break;
                    }
                    if child_h.size.value > 0 && !child_h.size.is_unknown {
                        file.seek(SeekFrom::Current(child_h.size.value as i64))?;
                    } else {
                        break;
                    }
                }

                clusters.push((pos, timestamp_ticks * timecode_scale));

                // Skip past this cluster's body
                if let Some(end) = body_end {
                    pos = end;
                } else {
                    // Unknown-size cluster: fall back to byte-scanning for the next one
                    match Self::scan_to_next_cluster_in(file, body_start, limit)? {
                        Some(next) => pos = next,
                        None => break,
                    }
                }
            } else if header.size.value > 0 && !header.size.is_unknown {
                // Non-cluster element: skip its body
                pos = body_start + header.size.value;
            } else {
                break;
            }
        }

        Ok(clusters)
    }

    // ── Keyframe caching (instance methods, existing logic preserved) ───

    fn update_cache(
        &mut self,
        track_num: u64,
        after: bool,
    ) -> Result<()> {
        let reference_timestamp_ns = self.reference_timestamp_ns;

        // Try to find keyframe in current cluster first
        let cluster = Cluster::from_file_pos(&mut self.file, self.position)?;
        let keyframe_idx_opt = if after {
            cluster.get_keyframe_after(track_num, reference_timestamp_ns, self.timecode_scale)
        } else {
            cluster.get_keyframe_before(track_num, reference_timestamp_ns, self.timecode_scale)
        };

        // If found a suitable keyframe in current cluster, use it
        if let Some(keyframe_idx) = keyframe_idx_opt {
            let keyframe_timestamp_ns = cluster
                .blocks
                .get(keyframe_idx)
                .ok_or(Error::InvalidConfig(
                    "Keyframe Index out of bounds".to_string(),
                ))?
                .timestamp_ns(cluster.timestamp.0 as i64, self.timecode_scale)?;
            
            // Verify it actually meets the criteria
            let meets_criteria = if after {
                keyframe_timestamp_ns >= reference_timestamp_ns
            } else {
                keyframe_timestamp_ns <= reference_timestamp_ns
            };
            
            if meets_criteria {
                self.keyframe_cluster_position
                    .insert((track_num, after), self.position);
                self.keyframe_timestamp_ns.insert(
                    (track_num, after),
                    keyframe_timestamp_ns,
                );
                return Ok(());
            }
        }

        // No suitable keyframe in current cluster, search neighboring clusters.
        // Use a generous limit — keyframe intervals can easily exceed 5–10 s on
        // streaming content, so we scan up to 60 clusters forward.
        const MAX_NEIGHBOR_SEARCH: usize = 60;
        
        // Scan to build a list of nearby cluster positions
        let nearby_clusters = self.scan_nearby_clusters(MAX_NEIGHBOR_SEARCH)?;
        
        // Search in the appropriate direction
        let search_clusters: Vec<(u64, u64)> = if after {
            // For "after", only consider clusters whose timestamp is at or after
            // the anchor cluster (so we actually scan *past* the reference).
            nearby_clusters.into_iter()
                .filter(|(_, ts)| *ts >= cluster.get_timestamp_ns(self.timecode_scale))
                .collect()
        } else {
            // For "before", use any cluster at or before the anchor.
            let mut before_clusters: Vec<_> = nearby_clusters.into_iter()
                .filter(|(_, ts)| *ts <= cluster.get_timestamp_ns(self.timecode_scale))
                .collect();
            before_clusters.reverse(); // nearest-first
            before_clusters
        };

        for (cluster_pos, _cluster_ts) in search_clusters {
            
            if let Ok(neighbor_cluster) = Cluster::from_file_pos(&mut self.file, cluster_pos) {
                let keyframe_idx_opt = if after {
                    neighbor_cluster.get_keyframe_after(track_num, reference_timestamp_ns, self.timecode_scale)
                } else {
                    neighbor_cluster.get_keyframe_before(track_num, reference_timestamp_ns, self.timecode_scale)
                };
                
                if let Some(keyframe_idx) = keyframe_idx_opt {
                    if let Ok(keyframe_timestamp_ns) = neighbor_cluster
                        .blocks
                        .get(keyframe_idx)
                        .ok_or(Error::InvalidConfig("Keyframe Index out of bounds".to_string()))?
                        .timestamp_ns(neighbor_cluster.timestamp.0 as i64, self.timecode_scale)
                    {
                        // Verify it meets the criteria
                        let meets_criteria = if after {
                            keyframe_timestamp_ns >= reference_timestamp_ns
                        } else {
                            keyframe_timestamp_ns <= reference_timestamp_ns
                        };
                        
                        if meets_criteria {
                            self.keyframe_cluster_position.insert(
                                (track_num, after),
                                cluster_pos,
                            );
                            self.keyframe_timestamp_ns.insert(
                                (track_num, after),
                                keyframe_timestamp_ns,
                            );
                            return Ok(());
                        }
                    }
                }
            }
        }

        // Fallback: use first/last keyframe from original cluster if no suitable keyframe found
        let all_keyframes = cluster.get_keyframes(track_num);
        let keyframe_idx: usize = if after {
            match all_keyframes.last() {
                Some(keyframe) => keyframe.clone(),
                None => return Err(Error::InvalidConfig("No keyframes found in any nearby cluster".to_string())),
            }
        } else {
            match all_keyframes.first() {
                Some(keyframe) => keyframe.clone(),
                None => return Err(Error::InvalidConfig("No keyframes found in any nearby cluster".to_string())),
            }
        };
        
        self.keyframe_cluster_position
            .insert((track_num, after), self.position);

        let keyframe_timestamp_ns = cluster
            .blocks
            .get(keyframe_idx)
            .ok_or(Error::InvalidConfig(
                "Keyframe Index out of bounds".to_string(),
            ))?
            .timestamp_ns(cluster.timestamp.0 as i64, self.timecode_scale)?;
        self.keyframe_timestamp_ns.insert(
            (track_num, after),
            keyframe_timestamp_ns,
        );

        Ok(())
    }

    /// Scan nearby clusters (up to max_count before and after current position)
    fn scan_nearby_clusters(&mut self, max_count: usize) -> Result<Vec<(u64, u64)>> {
        let mut clusters = Vec::new();
        let original_pos = self.file.stream_position()?;
        
        // Start from current cluster position and scan forward
        self.file.seek(SeekFrom::Start(self.position))?;
        
        for _ in 0..max_count {
            let pos = self.file.stream_position()?;
            if let Ok(header) = Header::read_from(&mut self.file) {
                if header.id == Cluster::ID {
                    if let Ok(cluster) = Cluster::read_element(&header, &mut self.file) {
                        let timestamp_ns = cluster.get_timestamp_ns(self.timecode_scale);
                        clusters.push((pos, timestamp_ns));
                    } else {
                        break;
                    }
                } else {
                    // Skip non-cluster elements
                    let size = header.size.value;
                    if size > 0 && !header.size.is_unknown {
                        if self.file.seek(SeekFrom::Current(size as i64)).is_err() {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        
        // Restore original position
        self.file.seek(SeekFrom::Start(original_pos))?;
        Ok(clusters)
    }
}
