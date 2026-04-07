use super::mkv_reader::MkvReader;
use crate::block_ext::{ClusterBlockExt, ClusterExt};
use crate::{Error, Result};
use log::trace;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::collections::HashMap;
use std::f32::consts::E;
use std::io::{Read, Seek, SeekFrom};

/// Lightweight cache for keyframe positions in the current cluster of interest
/// to speed up freeze seek operations without fully parsing all blocks in the cluster.
///
/// Created via binary search to find the cluster containing a given timestamp,
/// then lazily caches keyframe positions when queried.
pub struct KeyframePositionCache {
    pub position: u64,
    file: Box<dyn MkvReader>,
    timecode_scale: u64,
    reference_timestamp_ns: u64,
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
        mut file: Box<dyn MkvReader>,
        timecode_scale: u64,
        timestamp_ns: u64,
        range: Option<(u64, u64)>,
    ) -> Result<Self> {
        let (lo, hi) = match range {
            Some((start, end)) => (start, end),
            None => {
                let file_len = file.stream_length()?;
                (0, file_len)
            }
        };
        let start_time = std::time::Instant::now();
        let position =
            Self::binary_search_cluster(&mut file, timecode_scale, timestamp_ns as i64, lo, hi)?;
        trace!(
            "MkvRemuxer: Binary search for cluster completed in {} ms",
            start_time.elapsed().as_millis()
        );

        Ok(Self {
            position,
            file,
            timecode_scale,
            reference_timestamp_ns: timestamp_ns,
            keyframe_timestamp_ns: HashMap::new(),
            keyframe_cluster_position: HashMap::new(),
        })
    }

    pub fn form_file_pos(
        file: Box<dyn MkvReader>,
        timecode_scale: u64,
        timestamp_ns: u64,
        position: u64,
    ) -> Result<Self> {
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
        self.reference_timestamp_ns as i64
    }

    pub fn get_keyframe_timestamp_ns(&mut self, track_num: u64, after: bool) -> Result<i64> {
        if let Some(ts) = self.keyframe_timestamp_ns.get(&(track_num, after)) {
            Ok(*ts)
        } else {
            self.update_cache(track_num, after)?;
            let ts =
                self.keyframe_timestamp_ns
                    .get(&(track_num, after))
                    .ok_or(Error::InvalidConfig(
                        "Keyframe timestamp not found".to_string(),
                    ))?;
            Ok(*ts)
        }
    }

    pub fn get_closest_keyframe_timestamp_ns(&mut self, track_num: u64) -> Result<i64> {
        let after_ts = self.get_keyframe_timestamp_ns(track_num, true)?;
        let before_ts = self.get_keyframe_timestamp_ns(track_num, false)?;

        let after_diff = (after_ts - self.reference_timestamp_ns as i64).abs();
        let before_diff = (before_ts - self.reference_timestamp_ns as i64).abs();

        if after_diff < before_diff {
            Ok(after_ts)
        } else {
            Ok(before_ts)
        }
    }

    /// Returns the file position of the cluster that contains the keyframe
    /// nearest to the reference timestamp for the given `track_num` and direction.
    pub fn get_keyframe_cluster_position(&mut self, track_num: u64, after: bool) -> Result<u64> {
        if let Some(pos) = self.keyframe_cluster_position.get(&(track_num, after)) {
            Ok(*pos)
        } else {
            self.update_cache(track_num, after)?;
            let pos = self
                .keyframe_cluster_position
                .get(&(track_num, after))
                .ok_or(Error::NotFound(
                    "Keyframe cluster position not found".to_string(),
                ))?;
            Ok(*pos)
        }
    }

    // ── Binary-search helpers (static, operate on a bare File) ──────────

    /// Binary-search the file for the cluster whose timestamp is closest to
    /// (but ≤) `target_timestamp_ns`. Returns the file position of that cluster.
    fn binary_search_cluster(
        file: &mut dyn MkvReader,
        timecode_scale: u64,
        target_timestamp_ns: i64,
        mut lo: u64,
        mut hi: u64,
    ) -> Result<u64> {
        let target_unsigned = target_timestamp_ns.max(0) as u64;

        // lo and hi track the best-known cluster positions (or file bounds).
        // We converge until no new cluster is found between them.
        let mut lo_is_cluster = false;

        let mut old_mid: Option<u64> = None;
        let mut sanity_check_counter = 0;

        loop {
            if sanity_check_counter > 100 {
                return Err(Error::InternalBug(
                    "Binary search failed to converge after 100 iterations".to_string(),
                ));
            }
            sanity_check_counter += 1;

            let mid = lo + (hi - lo) / 2;

            // we convered mid in no longer changing
            if let Some(old_mid_val) = old_mid {
                if mid == old_mid_val {
                    // No progress in narrowing, break to avoid infinite loop
                    break;
                }
            }
            old_mid = Some(mid);

            match scan_cluster_in_direction(file, mid, Direction::Forward)? {
                Some(cluster_pos) => {
                    let time_stamp =
                        Self::read_cluster_timestamp_at(file, cluster_pos, timecode_scale)?;
                    if time_stamp <= target_unsigned {
                        lo = cluster_pos;
                        lo_is_cluster = true;
                    } else {
                        hi = cluster_pos;
                    }
                }
                _ => {
                    // No cluster found between lo and hi (or found one outside range).
                    // We might have reached the end of the file
                    hi = mid;
                }
            }
        }

        // we do a linear scan between lo and mid the check for additional clusters there
        let linear_scan_limit =
            old_mid.ok_or(Error::InternalBug("No mid value found".to_string()))?;
        let mut linear_scan_pos: u64 = if lo_is_cluster {
            lo
        } else {
            scan_cluster_in_direction(file, lo, Direction::Forward)?.ok_or(Error::FileCorrupted(
                "No cluster found in linaer scan".to_string(),
            ))?
        };
        let mut sanity_check_counter_2 = 0;
        // scan for next cluster that is <= target
        while linear_scan_pos < linear_scan_limit {
            match scan_cluster_in_direction(file, linear_scan_pos, Direction::Next)? {
                Some(cluster_pos) => {
                    let time_stamp =
                        Self::read_cluster_timestamp_at(file, cluster_pos, timecode_scale)?;
                    if time_stamp <= target_unsigned {
                        linear_scan_pos = cluster_pos;
                    } else {
                        break;
                    }
                }
                _ => break, // end of file
            }
            if sanity_check_counter_2 > 100 {
                return Err(Error::InternalBug(
                    "Linear scan failed to converge after 100 iterations".to_string(),
                ));
            }
            sanity_check_counter_2 += 1;
        }

        return Ok(linear_scan_pos);
    }

    /// Read only the Cluster timestamp at a given file position without parsing
    /// any block data. Reads a small buffer and parses EBML inline.
    /// Assumes `pos` points to a valid Cluster header.
    /// The file seek position is preserved after this operation.
    fn read_cluster_timestamp_at(
        file: &mut dyn MkvReader,
        pos: u64,
        timecode_scale: u64,
    ) -> Result<u64> {
        let old_file_pos = file.stream_position()?;
        file.seek(SeekFrom::Start(pos))?;
        // Cluster header (4 ID + up to 8 size) + Timestamp element is typically
        // within the first ~20 bytes. 64 bytes is plenty of headroom.
        let mut buf = [0u8; 64];
        let n = file.read(&mut buf)?;
        file.seek(SeekFrom::Start(old_file_pos))?;

        let buf = &buf[..n];

        // Skip Cluster element ID (4 bytes)
        let mut i: usize = 4;
        if i >= buf.len() {
            return Ok(0);
        }
        // Skip Cluster size (variable-length VINT)
        let size_width = ebml_vint_width(buf[i]);
        i += size_width;

        // Scan up to 5 child elements looking for Timestamp (ID 0xE7)
        for _ in 0..5 {
            if i >= buf.len() {
                break;
            }

            let id_width = ebml_vint_width(buf[i]);
            if i + id_width > buf.len() {
                break;
            }
            let is_timestamp = id_width == 1 && buf[i] == 0xE7;
            i += id_width;

            if i >= buf.len() {
                break;
            }
            let data_size_width = ebml_vint_width(buf[i]);
            if i + data_size_width > buf.len() {
                break;
            }
            let data_size = ebml_vint_value(&buf[i..]) as usize;
            i += data_size_width;

            if is_timestamp {
                if i + data_size > buf.len() {
                    break;
                }
                let mut ticks: u64 = 0;
                for &b in &buf[i..i + data_size] {
                    ticks = (ticks << 8) | b as u64;
                }
                return Ok(ticks * timecode_scale);
            }

            i += data_size;
        }

        // Timestamp not found (shouldn't happen in valid MKV), default to 0
        Err(Error::NotFound(
            "Cluster timestamp not found or invalid".to_string(),
        ))
    }

    // ── Keyframe caching (instance methods, existing logic preserved) ───

    fn update_cache(&mut self, track_num: u64, mut after: bool) -> Result<()> {
        let reference_timestamp_ns = self.reference_timestamp_ns;

        let m = 1_000_000_000.0f64;
        trace!(
            "MkvRemuxer: update_cache track={}, after={}, reference_ts={:.3}s, cluster_pos={}",
            track_num,
            after,
            reference_timestamp_ns as f64 / m,
            self.position
        );

        // Try to find keyframe in current cluster first
        let initial_cluster = Cluster::from_file_pos(&mut self.file, self.position)?;
        let initial_cluster_ts_ns = initial_cluster.timestamp.0 as i64 * self.timecode_scale as i64;
        trace!(
            "MkvRemuxer: update_cache current cluster timestamp={:.3}s, block_count={}",
            initial_cluster_ts_ns as f64 / m,
            initial_cluster.blocks.len()
        );

        // Log all keyframes in this cluster for the track
        for (i, block) in initial_cluster.blocks.iter().enumerate() {
            if let Ok(true) = block.is_keyframe() {
                if let Ok(tn) = block.track_number() {
                    if tn == track_num {
                        if let Ok(ts) = block
                            .timestamp_ns(initial_cluster.timestamp.0 as i64, self.timecode_scale)
                        {
                            trace!(
                                "MkvRemuxer: update_cache cluster keyframe[{}]: track={}, ts={:.3}s",
                                i,
                                tn,
                                ts as f64 / m
                            );
                        }
                    }
                }
            }
        }
        let mut current_after = after;
        let mut current_search_pos = self.position;
        let mut current_cluster = initial_cluster.clone();
        let mut direction_changed = false;
        let mut sanity_check_counter = 0;
        loop {
            if sanity_check_counter > 1000 {
                return Err(Error::FileCorrupted(format!(
                    "No keyframes found after scanning 1000 clusters for track {} reference_ts={:.3}s, filesize={}",
                    track_num,
                    reference_timestamp_ns as f64 / m,
                    self.file.stream_length().unwrap_or(0)
                )));
            }
            sanity_check_counter += 1;
            let keyframe_idx_opt = if current_after {
                current_cluster.get_keyframe_after(
                    track_num,
                    reference_timestamp_ns as i64,
                    self.timecode_scale,
                )
            } else {
                current_cluster.get_keyframe_before(
                    track_num,
                    reference_timestamp_ns as i64,
                    self.timecode_scale,
                )
            };

            // If found a suitable keyframe in current cluster, use it
            if let Some(keyframe_idx) = keyframe_idx_opt {
                let keyframe_timestamp_ns = current_cluster
                    .blocks
                    .get(keyframe_idx)
                    .ok_or(Error::InternalBug(
                        "Keyframe Index out of bounds".to_string(),
                    ))?
                    .timestamp_ns(current_cluster.timestamp.0 as i64, self.timecode_scale)?;

                // Verify it actually meets the criteria
                let meets_criteria = if current_after {
                    keyframe_timestamp_ns >= reference_timestamp_ns as i64
                } else {
                    keyframe_timestamp_ns <= reference_timestamp_ns as i64
                };

                trace!(
                    "MkvRemuxer: update_cache keyframe candidate: idx={}, ts={:.3}s, meets_criteria={}",
                    keyframe_idx,
                    keyframe_timestamp_ns as f64 / m,
                    meets_criteria
                );

                if meets_criteria {
                    self.keyframe_cluster_position
                        .insert((track_num, after), current_search_pos);
                    self.keyframe_timestamp_ns
                        .insert((track_num, after), keyframe_timestamp_ns);
                    trace!(
                        "MkvRemuxer: update_cache found keyframe at {:.3}s from cluster_pos={}",
                        keyframe_timestamp_ns as f64 / m,
                        current_search_pos
                    );
                    return Ok(());
                }
            } else {
                trace!(
                    "MkvRemuxer: update_cache no keyframe found in cluster for track={}, after={}",
                    track_num, after
                );
            }

            // No suitable keyframe in current cluster, search neighboring clusters
            // using scan_cluster_in_direction.
            let direction = if current_after {
                Direction::Next
            } else {
                Direction::Previous
            };
            let (cluster_pos, neighbor_cluster) =
                match scan_cluster_in_direction(&mut self.file, current_search_pos, direction)? {
                    Some(result) => {
                        let cluster = Cluster::from_file_pos(&mut self.file, result)?;
                        (result, cluster)
                    }
                    None => {
                        // we have reached a file end so reverse and search to get at least a keyframe that is close to the reference timestamp
                        // we dont want to ""ping pong" indefinitely
                        if direction_changed {
                            return Err(Error::FileCorrupted(
                                "No keyframes found after scanning all clusters".to_string(),
                            ));
                        } else {
                            current_after = !current_after;
                            current_search_pos = self.position;
                            current_cluster = initial_cluster.clone();
                            direction_changed = true;
                            continue;
                        }
                    }
                };
            current_cluster = neighbor_cluster;
            current_search_pos = cluster_pos;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// scan to next cluster but ignore the current
    Next,
    /// scan to previous cluster but ignore the current
    Previous,
    /// scan forward to next cluster including the current
    Forward,
    /// scan backward to previous cluster including the current
    Backward,
}

/// returns (position_of_cluster_in_file) of the next or previous cluster of the specified position
/// The position can be in the middle of a cluster and does not have top point to a valid cluster header
/// IF the pos points to a valid cluster header, this cluster will be skipped and the next/previous one will be returned
/// The files seek position is preseved after this operation
fn scan_cluster_in_direction(
    file: &mut dyn MkvReader,
    pos: u64,
    direction: Direction,
) -> Result<Option<u64>> {
    const BUF_SIZE: usize = 819200; // 800KB buffer for scanning (should be enough to find a cluster header in most cases, adjust as needed)
    const BUFFER_OVER_SCAN: usize = 16; // number of bytes to overscan we need to overscan a whole cluster header (max 8 bytes id + 8 bytes length) to avoid missing cluster headers that are at the end of the buffer
    let old_file_pos = file.stream_position()?;
    let file_end_pos = file.stream_length()?;

    fn read_cluster_headers_pos_from_buffer(buf: &[u8]) -> Vec<usize> {
        const CLUSTER_ID: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
        let mut cluster_starts: Vec<usize> = buf
            .windows(4)
            .enumerate()
            .filter_map(|(i, w)| if w == CLUSTER_ID { Some(i) } else { None })
            .collect();
        // after a more rudimentary matching try to parse the header to avoid false positives in the block data, we need to retain only the positions that are actual cluster headers
        let mut buf_reader = std::io::Cursor::new(buf);
        cluster_starts.retain(|&pos| {
            buf_reader.set_position(pos as u64);
            match Header::read_from(&mut buf_reader) {
                Ok(h) => h.id == Cluster::ID,
                Err(_) => false,
            }
        });
        cluster_starts
    }

    // require absolute positions of candidate clusters
    fn filter_valid_cluster_pos(
        direction: Direction,
        mut candidate_pos: Vec<u64>,
        initial_position: u64,
    ) -> Result<Option<u64>> {
        // we need to travers backward if the direction is backward to find the nearest cluster header
        candidate_pos.sort();
        // for back looking scans we invert the iterator
        let iterator: Box<dyn Iterator<Item = &u64>> = match direction {
            Direction::Forward | Direction::Next => Box::new(candidate_pos.iter()),
            Direction::Backward | Direction::Previous => Box::new(candidate_pos.iter().rev()),
        };
        // we only allow the initial considers for Forward and Backward
        let my_cmp: fn(u64, u64) -> bool = match direction {
            Direction::Forward => |candidate, initial| candidate >= initial, // can be initial or greater
            Direction::Next => |candidate, initial| candidate > initial, // must be greater than initial
            Direction::Backward => |candidate, initial| candidate <= initial, // can be initial or smaller
            Direction::Previous => |candidate, initial| candidate < initial, // must be smaller than initial
        };
        let mut output_position: Option<u64> = None;
        for pos in iterator {
            if my_cmp(*pos, initial_position) {
                output_position = Some(*pos);
                break;
            }
        }
        Ok(output_position)
    }

    let mut current_position = match direction {
        Direction::Forward | Direction::Next => pos,
        Direction::Backward | Direction::Previous => {
            pos.saturating_sub((BUF_SIZE - BUFFER_OVER_SCAN) as u64)
        }
    };
    let mut buffer = [0u8; BUF_SIZE];
    let mut output_position: Option<u64> = None;
    let mut sanity_check_counter = 0;
    while current_position > 0 && current_position < file_end_pos {
        if sanity_check_counter > 200_000 {
            // approx 1600MB scanned without finding a valid cluster header
            return Err(Error::FileCorrupted(format!(
                "Could not find a valid Cluster header after scanning {} bytes",
                sanity_check_counter * BUF_SIZE
            )));
        }
        sanity_check_counter += 1;
        file.seek(SeekFrom::Start(current_position))?;
        let n = file.read(&mut buffer)?;
        if n <= BUFFER_OVER_SCAN {
            // not enough data to contain a cluster header, stop searching
            break;
        }
        let cluster_positions_n_buffer = read_cluster_headers_pos_from_buffer(&buffer[..n]);
        // convert to absolute positions and exclude the position we want to skip
        let cluster_positions_absolute: Vec<u64> = cluster_positions_n_buffer
            .iter()
            .map(|offset| current_position + *offset as u64)
            .collect();
        if let Some(cluster_absolute_pos) =
            filter_valid_cluster_pos(direction, cluster_positions_absolute, pos)?
        {
            output_position = Some(cluster_absolute_pos);
            break;
        } else {
            // no valid cluster header found in this buffer, continue searching
            match direction {
                // overscan to avoid missing cluster headers
                Direction::Forward | Direction::Next => {
                    current_position += (n as u64 - BUFFER_OVER_SCAN as u64)
                }
                Direction::Backward | Direction::Previous => {
                    current_position =
                        current_position.saturating_sub(BUF_SIZE as u64 - BUFFER_OVER_SCAN as u64)
                }
            }
        }
        if n < BUF_SIZE {
            // reached end of file
            break;
        }
    }
    file.seek(SeekFrom::Start(old_file_pos))?;
    if let Some(out_pos) = output_position {
        Ok(Some(out_pos))
    } else {
        Ok(None)
    }
}

/// Returns the byte-width of an EBML variable-length integer given its first byte.
fn ebml_vint_width(first_byte: u8) -> usize {
    if first_byte == 0 {
        return 8;
    }
    first_byte.leading_zeros() as usize + 1
}

/// Parses the numeric value of an EBML VINT from a byte slice (stripping the width marker bits).
fn ebml_vint_value(buf: &[u8]) -> u64 {
    let width = ebml_vint_width(buf[0]);
    let mut val = (buf[0] & (0xFF >> width)) as u64;
    for i in 1..width.min(buf.len()) {
        val = (val << 8) | buf[i] as u64;
    }
    val
}
