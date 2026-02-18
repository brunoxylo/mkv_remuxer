use crate::source::FileSource;
use crate::source::InputSource;
use crate::source::Uninitialized;
use crate::cluster_warpper::{CLUSTER_MAX_DURATION_NS, CLUSTER_MAX_SIZE_BYTES};
use crate::block_ext::ClusterBlockExt;
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::collections::HashSet;
use mkv_element::prelude::*;
use mkv_element::io::blocking_impl::*;


pub fn test_file_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test.webm")
}

pub fn sources_implementations() -> Vec<InputSource<Uninitialized>> {
    vec![
        FileSource::new(test_file_path()).unwrap().into(),
        // Add other Source implementations here as needed
    ]
}

/// Get the duration metadata from an input MKV/WebM file
///
/// # Arguments
/// * `file_path` - Path to the input file
///
/// # Returns
/// * Duration in nanoseconds if present in metadata
pub fn get_input_duration_ns<P: AsRef<Path>>(file_path: P) -> Result<Option<u64>> {
    let mut file = File::open(file_path.as_ref())?;
    
    // Read EBML header
    let ebml_header = Header::read_from(&mut file)?;
    if ebml_header.id.value != Ebml::ID.value {
        return Err(Error::InvalidConfig("Not an EBML file".to_string()));
    }
    Ebml::read_element(&ebml_header, &mut file)?;
    
    // Read Segment header
    let segment_header = Header::read_from(&mut file)?;
    if segment_header.id.value != Segment::ID.value {
        return Err(Error::InvalidConfig("Not a valid Segment".to_string()));
    }
    
    let mut timecode_scale = 1_000_000u64;
    let file_len = file.metadata()?.len();
    let mut position = file.stream_position()?;
    
    // Look for Info element
    while position < file_len {
        let header = match Header::read_from(&mut file) {
            Ok(h) => h,
            Err(_) => break,
        };
        
        if header.id.value == Info::ID.value {
            let info = Info::read_element(&header, &mut file)?;
            timecode_scale = info.timestamp_scale.0;
            if let Some(duration) = info.duration {
                return Ok(Some((duration.0 * timecode_scale as f64) as u64));
            }
            return Ok(None);
        }
        
        // Skip other elements
        if !header.size.is_unknown && header.size.value > 0 {
            file.seek(SeekFrom::Current(header.size.value as i64))?;
        } else {
            break;
        }
        
        position = file.stream_position()?;
    }
    
    Ok(None)
}

/// Validation report for an MKV file
#[derive(Debug, Clone)]
pub struct MkvValidationReport {
    pub syntax_valid: bool,
    pub timestamps_plausible: bool,
    pub all_tracks_present: bool,
    pub cluster_duration_valid: bool,
    pub cluster_size_valid: bool,
    pub cues_valid: bool,
    pub duration_valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub stats: ValidationStats,
}

#[derive(Debug, Clone, Default)]
pub struct ValidationStats {
    pub total_clusters: usize,
    pub total_blocks: usize,
    pub tracks_found: HashSet<u64>,
    pub max_cluster_duration_ns: u64,
    pub max_cluster_size_bytes: u64,
    pub cue_points_checked: usize,
    pub output_duration_ns: Option<u64>,
    pub expected_duration_ns: Option<u64>,
}

/// Validates an MKV output file for:
/// 1. EBML syntax errors
/// 2. Monotonically increasing timestamps
/// 3. All tracks in metadata are present in clusters
/// 4. Cluster duration and size constraints
/// 5. Cues point to correct clusters
/// 6. Output duration metadata matches expected input duration (if provided)
///
/// # Arguments
/// * `file_path` - Path to the MKV file to validate
/// * `require_cues` - Whether cues are required to be present
/// * `expected_input_duration_ns` - Expected input file duration in nanoseconds (for comparison)
/// * `require_strict_monotonic` - If true, requires strictly monotonic timestamps. If false, only checks plausibility (no jumps > CLUSTER_MAX_DURATION_NS)
/// * `check_cluster_limits` - If true, validates cluster duration and size limits. If false, skips these checks
///
/// # Returns
/// * `MkvValidationReport` - Detailed validation report
pub fn validate_mkv_output<P: AsRef<Path>>(
    file_path: P, 
    require_cues: bool,
    expected_input_duration_ns: Option<u64>,
    require_strict_monotonic: bool,
    check_cluster_limits: bool
) -> Result<MkvValidationReport> {
    let mut report = MkvValidationReport {
        syntax_valid: true,
        timestamps_plausible: true,
        all_tracks_present: false,
        cluster_duration_valid: true,
        cluster_size_valid: true,
        cues_valid: true,
        duration_valid: true,
        errors: Vec::new(),
        warnings: Vec::new(),
        stats: ValidationStats {
            expected_duration_ns: expected_input_duration_ns,
            ..Default::default()
        },
    };

    let mut file = File::open(file_path.as_ref()).map_err(|e| {
        report.syntax_valid = false;
        report.errors.push(format!("Failed to open file: {}", e));
        Error::Io(e)
    })?;

    // Read EBML header
    let ebml_header = match Header::read_from(&mut file) {
        Ok(h) => h,
        Err(e) => {
            report.syntax_valid = false;
            report.errors.push(format!("Failed to read EBML header: {:?}", e));
            return Ok(report);
        }
    };

    if ebml_header.id.value != Ebml::ID.value {
        report.syntax_valid = false;
        report.errors.push("Not an EBML file".to_string());
        return Ok(report);
    }

    // Validate EBML element
    if let Err(e) = Ebml::read_element(&ebml_header, &mut file) {
        report.syntax_valid = false;
        report.errors.push(format!("Failed to parse EBML body: {:?}", e));
        return Ok(report);
    }

    // Read Segment header
    let segment_header = match Header::read_from(&mut file) {
        Ok(h) => h,
        Err(e) => {
            report.syntax_valid = false;
            report.errors.push(format!("Failed to read Segment header: {:?}", e));
            return Ok(report);
        }
    };

    if segment_header.id.value != Segment::ID.value {
        report.syntax_valid = false;
        report.errors.push(format!(
            "Expected Segment element, found ID: {:x}",
            segment_header.id.value
        ));
        return Ok(report);
    }

    // Track parsing state
    let mut timecode_scale = 1_000_000u64; // Default 1ms
    let mut expected_tracks: HashSet<u64> = HashSet::new();
    let mut cluster_positions: Vec<(u64, u64)> = Vec::new(); // (timestamp, file_position)
    let mut cues: Option<Cues> = None;
    let mut last_timestamp_ns: Option<i64> = None;

    let file_len = file.metadata()?.len();
    let segment_data_start = file.stream_position()?; // Position where Segment's data starts
    let mut position = segment_data_start;

    // First pass: parse all elements
    while position < file_len {
        let header = match Header::read_from(&mut file) {
            Ok(h) => h,
            Err(_) => break, // EOF
        };

        match header.id.value {
            id if id == Info::ID.value => {
                match Info::read_element(&header, &mut file) {
                    Ok(info) => {
                        timecode_scale = info.timestamp_scale.0;
                        // Capture output duration from metadata
                        if let Some(duration) = info.duration {
                            report.stats.output_duration_ns = Some((duration.0 * timecode_scale as f64) as u64);
                        }
                    }
                    Err(e) => {
                        report.syntax_valid = false;
                        report.errors.push(format!("Failed to parse Info: {:?}", e));
                    }
                }
            }
            id if id == Tracks::ID.value => {
                match Tracks::read_element(&header, &mut file) {
                    Ok(tracks) => {
                        for entry in &tracks.track_entry {
                            expected_tracks.insert(entry.track_number.0);
                        }
                    }
                    Err(e) => {
                        report.syntax_valid = false;
                        report.errors.push(format!("Failed to parse Tracks: {:?}", e));
                    }
                }
            }
            id if id == Cues::ID.value => {
                match Cues::read_element(&header, &mut file) {
                    Ok(parsed_cues) => {
                        cues = Some(parsed_cues);
                    }
                    Err(e) => {
                        report.syntax_valid = false;
                        report.errors.push(format!("Failed to parse Cues: {:?}", e));
                    }
                }
            }
            id if id == Cluster::ID.value => {
                let cluster_start_pos = position;
                let cluster_size = if header.size.is_unknown {
                    report.warnings.push("Cluster has unknown size".to_string());
                    0
                } else {
                    header.size.value
                };

                match Cluster::read_element(&header, &mut file) {
                    Ok(cluster) => {
                        report.stats.total_clusters += 1;
                        
                        let cluster_timestamp = cluster.timestamp.0;
                        cluster_positions.push((cluster_timestamp, cluster_start_pos));

                        // Check cluster size constraint
                        if check_cluster_limits && cluster_size > CLUSTER_MAX_SIZE_BYTES {
                            report.cluster_size_valid = false;
                            report.errors.push(format!(
                                "Cluster {} exceeds max size: {} > {} bytes",
                                report.stats.total_clusters,
                                cluster_size,
                                CLUSTER_MAX_SIZE_BYTES
                            ));
                        }
                        report.stats.max_cluster_size_bytes = 
                            report.stats.max_cluster_size_bytes.max(cluster_size);

                        // Process blocks
                        let mut cluster_min_ts_ns = i64::MAX;
                        let mut cluster_max_ts_ns = i64::MIN;

                        for block in &cluster.blocks {
                            report.stats.total_blocks += 1;

                            // Get track number
                            if let Ok(track_num) = block.track_number() {
                                report.stats.tracks_found.insert(track_num);
                            }

                            // Get absolute timestamp
                            if let Ok(block_ts_ns) = block.timestamp_ns(cluster_timestamp as i64, timecode_scale) {
                                cluster_min_ts_ns = cluster_min_ts_ns.min(block_ts_ns);
                                cluster_max_ts_ns = cluster_max_ts_ns.max(block_ts_ns);

                                // Check timestamp plausibility
                                if let Some(last_ts) = last_timestamp_ns {
                                    if require_strict_monotonic {
                                        // Strict monotonic check
                                        if block_ts_ns < last_ts {
                                            report.timestamps_plausible = false;
                                            report.errors.push(format!(
                                                "Non-monotonic timestamp: {} < {} in cluster {}",
                                                block_ts_ns,
                                                last_ts,
                                                report.stats.total_clusters
                                            ));
                                        }
                                    } else {
                                        // Plausibility check: timestamps shouldn't jump forward more than CLUSTER_MAX_DURATION_NS
                                        let timestamp_diff = block_ts_ns - last_ts;
                                        if timestamp_diff.abs() as u64 > CLUSTER_MAX_DURATION_NS {
                                            report.timestamps_plausible = false;
                                            report.errors.push(format!(
                                                "Implausible timestamp jump: {} ns ({:.2}s) between {} and {} in cluster {}",
                                                timestamp_diff.abs(),
                                                timestamp_diff.abs() as f64 / 1_000_000_000.0,
                                                last_ts,
                                                block_ts_ns,
                                                report.stats.total_clusters
                                            ));
                                        }
                                    }
                                }
                                last_timestamp_ns = Some(block_ts_ns);
                            }
                        }

                        // Check cluster duration constraint
                        if cluster_max_ts_ns > cluster_min_ts_ns {
                            let cluster_duration_ns = (cluster_max_ts_ns - cluster_min_ts_ns) as u64;
                            report.stats.max_cluster_duration_ns = 
                                report.stats.max_cluster_duration_ns.max(cluster_duration_ns);

                            if check_cluster_limits && cluster_duration_ns > CLUSTER_MAX_DURATION_NS {
                                report.cluster_duration_valid = false;
                                report.errors.push(format!(
                                    "Cluster {} exceeds max duration: {} > {} ns ({:.2}s > {:.2}s)",
                                    report.stats.total_clusters,
                                    cluster_duration_ns,
                                    CLUSTER_MAX_DURATION_NS,
                                    cluster_duration_ns as f64 / 1_000_000_000.0,
                                    CLUSTER_MAX_DURATION_NS as f64 / 1_000_000_000.0
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        report.syntax_valid = false;
                        report.errors.push(format!(
                            "Failed to parse Cluster at position {}: {:?}",
                            cluster_start_pos, e
                        ));
                    }
                }
            }
            _ => {
                // Skip unknown elements
                if !header.size.is_unknown && header.size.value > 0 {
                    file.seek(SeekFrom::Current(header.size.value as i64))?;
                } else {
                    break;
                }
            }
        }

        position = file.stream_position()?;
    }

    // Check if all expected tracks are present
    report.all_tracks_present = expected_tracks.is_subset(&report.stats.tracks_found);
    if !report.all_tracks_present {
        let missing: Vec<_> = expected_tracks
            .difference(&report.stats.tracks_found)
            .collect();
        report.errors.push(format!(
            "Missing tracks in clusters: {:?}",
            missing
        ));
    }

    // Validate output duration against expected input duration
    if let Some(expected_dur) = expected_input_duration_ns {
        if let Some(output_dur) = report.stats.output_duration_ns {
            // Output duration should be <= expected input duration (accounting for cuts)
            // Allow small tolerance for rounding (1ms)
            let tolerance_ns = 1_000_000u64; // 1ms
            if output_dur > expected_dur + tolerance_ns {
                report.duration_valid = false;
                report.errors.push(format!(
                    "Output duration ({:.3}s) exceeds expected input duration ({:.3}s)",
                    output_dur as f64 / 1_000_000_000.0,
                    expected_dur as f64 / 1_000_000_000.0
                ));
            }
        } else {
            report.warnings.push("Output file has no duration metadata to validate".to_string());
        }
    }

    // Validate cues if present
    if cues.is_none() && require_cues {
        report.cues_valid = false;
        report.errors.push("Cues element is required but not found".to_string());
    }
    if let Some(cues_data) = cues {
        validate_cues(&mut report, &cues_data, &cluster_positions, &mut file, segment_data_start)?;
    } else {
        report.warnings.push("No Cues element found in file".to_string());
    }
    if report.stats.total_clusters == 0 {
        report.errors.push("No clusters found in file".to_string());
    }
    if report.stats.total_blocks == 0 {
        report.errors.push("No blocks found in file".to_string());
    }


    Ok(report)
}

/// Validates that cues point to correct clusters
fn validate_cues(
    report: &mut MkvValidationReport,
    cues: &Cues,
    cluster_positions: &[(u64, u64)],
    file: &mut File,
    segment_data_start: u64,
) -> Result<()> {
    for cue_point in &cues.cue_point {
        report.stats.cue_points_checked += 1;
        let cue_time = cue_point.cue_time.0;

        for cue_track_pos in &cue_point.cue_track_positions {
            let cue_cluster_pos = cue_track_pos.cue_cluster_position.0;
            // Cue positions are relative to Segment data start, convert to absolute file position
            let absolute_cluster_pos = segment_data_start + cue_cluster_pos;

            // Find the cluster at this position
            let cluster_info = cluster_positions
                .iter()
                .find(|(_, pos)| *pos == absolute_cluster_pos);

            match cluster_info {
                Some((cluster_timestamp, _)) => {
                    // Check if cue time is reasonably close to cluster timestamp
                    // Note: Exact matches are not required in MKV - cues point to clusters near the seek time
                    let time_diff = (*cluster_timestamp as i64 - cue_time as i64).abs() as u64;
                    // Allow up to 1 second tolerance (1000 ticks at 1ms timescale)
                    if time_diff > 1000 {
                        report.warnings.push(format!(
                            "Cue time {} differs significantly from cluster timestamp {} (diff: {} ticks)",
                            cue_time, cluster_timestamp, time_diff
                        ));
                    }

                    // Verify cluster actually exists at that position
                    file.seek(SeekFrom::Start(absolute_cluster_pos))?;
                    match Header::read_from(file) {
                        Ok(header) => {
                            if header.id.value != Cluster::ID.value {
                                report.cues_valid = false;
                                report.errors.push(format!(
                                    "Cue points to position {} (absolute: {}) but no Cluster found (ID: {:x})",
                                    cue_cluster_pos, absolute_cluster_pos, header.id.value
                                ));
                            }
                        }
                        Err(e) => {
                            report.cues_valid = false;
                            report.errors.push(format!(
                                "Failed to read element at cue position {} (absolute: {}): {:?}",
                                cue_cluster_pos, absolute_cluster_pos, e
                            ));
                        }
                    }
                }
                None => {
                    report.cues_valid = false;
                    report.errors.push(format!(
                        "Cue points to cluster at position {} (absolute: {}) which was not found during parsing",
                        cue_cluster_pos, absolute_cluster_pos
                    ));
                }
            }
        }
    }

    Ok(())
}

impl MkvValidationReport {
    /// Returns true if all validations passed
    pub fn is_valid(&self) -> bool {
        self.syntax_valid
            && self.timestamps_plausible
            && self.all_tracks_present
            && self.cluster_duration_valid
            && self.cluster_size_valid
            && self.cues_valid
            && self.duration_valid
            && self.stats.total_clusters > 0
            && self.stats.total_blocks > 0
    }

    /// Returns a summary string of the validation
    pub fn summary(&self) -> String {
        let output_dur_str = self.stats.output_duration_ns
            .map(|d| format!("{:.3}s", d as f64 / 1_000_000_000.0))
            .unwrap_or_else(|| "N/A".to_string());
        let expected_dur_str = self.stats.expected_duration_ns
            .map(|d| format!("{:.3}s", d as f64 / 1_000_000_000.0))
            .unwrap_or_else(|| "N/A".to_string());
        
        format!(
            "MKV Validation Report:\n\
             - Syntax Valid: {}\n\
             - Timestamps Plausible: {}\n\
             - All Tracks Present: {}\n\
             - Cluster Duration Valid: {}\n\
             - Cluster Size Valid: {}\n\
             - Cues Valid: {}\n\
             - Duration Valid: {}\n\
             - Total Clusters: {}\n\
             - Total Blocks: {}\n\
             - Tracks Found: {:?}\n\
             - Max Cluster Duration: {:.2}s\n\
             - Max Cluster Size: {:.2} MB\n\
             - Output Duration (metadata): {}\n\
             - Expected Input Duration: {}\n\
             - Cue Points Checked: {}\n\
             - Errors: {}\n\
             - Warnings: {}",
            self.syntax_valid,
            self.timestamps_plausible,
            self.all_tracks_present,
            self.cluster_duration_valid,
            self.cluster_size_valid,
            self.cues_valid,
            self.duration_valid,
            self.stats.total_clusters,
            self.stats.total_blocks,
            self.stats.tracks_found,
            self.stats.max_cluster_duration_ns as f64 / 1_000_000_000.0,
            self.stats.max_cluster_size_bytes as f64 / 1_000_000.0,
            output_dur_str,
            expected_dur_str,
            self.stats.cue_points_checked,
            self.errors.len(),
            self.warnings.len()
        )
    }
}
