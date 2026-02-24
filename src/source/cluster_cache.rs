use crate::{Error, Result};
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use crate::block_ext::{ClusterBlockExt, ClusterExt};

/// Lightweight cache for keyframe positions in the current cluster of interest
/// to speed up freeze seek operations without fully parsing all blocks in the cluster
pub struct KeyframePositionCache {
    pub position: u64,
    file: File,
    timecode_scale: u64,
    keyframe_timestamp_ns: HashMap<(u64, i64, bool), i64>, // (track_num, reference_timestamp_ns, after or before) -> timestamp_ns of keyframe
    keyframe_cluster_position: HashMap<(u64, i64, bool), u64>, // (track_num, reference_timestamp_ns, after or before) -> file position of the cluster that holds the keyframe
}

impl KeyframePositionCache {
    pub fn new(position: u64, file: File, timecode_scale: u64) -> Self {
        Self {
            position,
            file,
            timecode_scale,
            keyframe_timestamp_ns: HashMap::new(),
            keyframe_cluster_position: HashMap::new(),
        }
    }
    
    pub fn set_pos(&mut self, position: u64) {
        self.position = position;
        self.keyframe_timestamp_ns.clear();
        self.keyframe_cluster_position.clear();
    }
    
    pub fn get_keyframe_timestamp_ns(
        &mut self,
        track_num: u64,
        reference_timestamp_ns: i64,
        after: bool,
    ) -> Result<i64> {
        if let Some(ts) =
            self.keyframe_timestamp_ns
                .get(&(track_num, reference_timestamp_ns, after))
        {
            Ok(*ts)
        } else {
            self.update_cache(track_num, after, reference_timestamp_ns)?;
            let ts = self
                .keyframe_timestamp_ns
                .get(&(track_num, reference_timestamp_ns, after))
                .ok_or(Error::InvalidConfig(
                    "Keyframe timestamp not found".to_string(),
                ))?;
            Ok(*ts)
        }
    }

    pub fn get_closest_keyframe_timestamp_ns(
        &mut self,
        track_num: u64,
        reference_timestamp_ns: i64,
    ) -> Result<i64> {
        let after_ts = self.get_keyframe_timestamp_ns(track_num, reference_timestamp_ns, true)?;
        let before_ts = self.get_keyframe_timestamp_ns(track_num, reference_timestamp_ns, false)?;

        let after_diff = (after_ts - reference_timestamp_ns).abs();
        let before_diff = (before_ts - reference_timestamp_ns).abs();

        if after_diff < before_diff {
            Ok(after_ts)
        } else {
            Ok(before_ts)
        }
    }

    /// Returns the file position of the cluster that contains the keyframe
    /// nearest to `reference_timestamp_ns` for the given `track_num` and direction.
    pub fn get_keyframe_cluster_position(
        &mut self,
        track_num: u64,
        after: bool,
        reference_timestamp_ns: i64,
    ) -> Result<u64> {
        if let Some(pos) =
            self.keyframe_cluster_position
                .get(&(track_num, reference_timestamp_ns, after))
        {
            Ok(*pos)
        } else {
            self.update_cache(track_num, after, reference_timestamp_ns)?;
            let pos = self
                .keyframe_cluster_position
                .get(&(track_num, reference_timestamp_ns, after))
                .ok_or(Error::InvalidConfig(
                    "Keyframe cluster position not found".to_string(),
                ))?;
            Ok(*pos)
        }
    }

    fn update_cache(
        &mut self,
        track_num: u64,
        after: bool,
        reference_timestamp_ns: i64,
    ) -> Result<()> {
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
                    .insert((track_num, reference_timestamp_ns, after), self.position);
                self.keyframe_timestamp_ns.insert(
                    (track_num, reference_timestamp_ns, after),
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
                .filter(|(_, ts)| *ts >= cluster.get_timestamp_ms(self.timecode_scale))
                .collect()
        } else {
            // For "before", use any cluster at or before the anchor.
            let mut before_clusters: Vec<_> = nearby_clusters.into_iter()
                .filter(|(_, ts)| *ts <= cluster.get_timestamp_ms(self.timecode_scale))
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
                                (track_num, reference_timestamp_ns, after),
                                cluster_pos,
                            );
                            self.keyframe_timestamp_ns.insert(
                                (track_num, reference_timestamp_ns, after),
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
            .insert((track_num, reference_timestamp_ns, after), self.position);

        let keyframe_timestamp_ns = cluster
            .blocks
            .get(keyframe_idx)
            .ok_or(Error::InvalidConfig(
                "Keyframe Index out of bounds".to_string(),
            ))?
            .timestamp_ns(cluster.timestamp.0 as i64, self.timecode_scale)?;
        self.keyframe_timestamp_ns.insert(
            (track_num, reference_timestamp_ns, after),
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
                        let timestamp_ns = cluster.get_timestamp_ms(self.timecode_scale);
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
