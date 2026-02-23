use crate::Error;
use crate::{
    Cluster, ClusterBlockExt, ClusterReadWrapper, ClusterWriteWrapper, Result, SourcesMappings,
};
use log::{debug, warn};
use mkv_element::{ClusterBlock, prelude::*};

pub struct MeltingPot {
    sources_mappings: SourcesMappings,
    clusters: Vec<Option<ClusterReadWrapper>>
}

impl MeltingPot {
    pub fn new(sources_mappings: SourcesMappings) -> Self {
        let num_sources = sources_mappings.sources.len();
        // Initialize empty vector and fill with None
        let initial_clusters = (0..num_sources).map(|_| None).collect();
        Self {
            sources_mappings,
            clusters: initial_clusters
        }
    }
    pub fn generate_next_cluster(&mut self) -> Result<Option<Cluster>> {
        let timescale = self.sources_mappings.get_time_scale()?;
        let mut iteration = 0;
        let mut output_cluster: Option<ClusterWriteWrapper> = None;

        loop {
            iteration += 1;
            if iteration % 10000 == 0 {
                warn!(
                    "MeltingPot loop iteration {}, this might indicate a problem",
                    iteration
                );
            }
            let mut all_sources_finished = true;
            // Get the next cluster from each source if not already
            for (index, source) in self.sources_mappings.sources.iter_mut().enumerate() {
                if self.clusters[index].is_none() {
                    self.clusters[index] = source.get_next_cluster()?.map(ClusterReadWrapper::new);
                }
                if self.clusters[index].is_some() {
                    all_sources_finished = false;
                }
            }

            // condition to exit loop
            if all_sources_finished {
                if let Some(o_cluster) = output_cluster {
                    if o_cluster.is_empty() {
                        debug!("No more clusters available");
                        return Ok(None);
                    } else {
                        return Ok(Some(o_cluster.finish()));
                    }
                } else {
                    debug!("No more clusters available");
                    return Ok(None);
                }
            }

            // find the block with the lowest timestamp among all input clusters
            let mut lowest_timestamp_ns: i64 = i64::MAX;
            let mut lowest_cluster_index = None;
            for (index, cluster_wrapper) in self.clusters.iter_mut().enumerate() {
                if let Some(cluster) = cluster_wrapper {
                    if cluster.is_empty() {
                        // Cluster has no more blocks, mark as finished
                        *cluster_wrapper = None;
                        debug!("Cluster {} reached end", index);
                        continue;
                    }
                    match cluster.get_current_absolute_timestamp_ns(timescale) {
                        Ok(ts) => {
                            if ts <= lowest_timestamp_ns {
                                lowest_timestamp_ns = ts;
                                lowest_cluster_index = Some(index);
                            }
                        }
                        Err(Error::InvalidBlockData(_)) => {
                            *cluster_wrapper = None; // treat cluster with invalid block data as finished
                            debug!("Cluster {} reached end", index);
                        }
                        Err(e) => {
                            warn!(
                                "Failed to get current absolute timestamp for cluster {}: {:?}",
                                index, e
                            );
                        }
                    }
                }
            }
            // not yet initialized
            if output_cluster.is_none() && lowest_cluster_index.is_some() {
                output_cluster = Some(ClusterWriteWrapper::new(
                    lowest_timestamp_ns as u64,
                    timescale,
                ));
            }

            // add pending block to output cluster if it exists

            if let Some(mut o_cluster) = output_cluster.take() {
                // add the block with the lowest timestamp to the output cluster
                if let Some(lowest_index) = lowest_cluster_index {
                    if let Some(input_cluster) = &mut self.clusters[lowest_index] {
                        if let Some(block) = input_cluster.next() {
                            let input_track_index = block.track_number()?;
                            // only add the block to the output cluster if its track is mapped to an output track (otherwise we just skip it)
                            if let Some(output_trackindex) = self
                                .sources_mappings
                                .is_track_mapped(lowest_index as u64, input_track_index)
                            {
                                match o_cluster.add_block(&block, lowest_timestamp_ns, Some(output_trackindex)) {
                                    Ok(_) => {}
                                    Err(Error::ClusterIsFull(_)) => {
                                        // Cluster is full, break the inner loop and start a new cluster
                                        input_cluster.step_back(); // step back to reprocess this block in the next cluster
                                        return Ok(Some(o_cluster.finish()));
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                        } else {
                            // No more blocks in this cluster, set to None to fetch next cluster from source in next iteration
                            self.clusters[lowest_index] = None;
                        }
                    }
                } else {
                    // No valid block found but sources aren't finished - this shouldn't happen
                    debug!(
                        "No valid block found in iteration {}, all_sources_finished={}",
                        iteration, all_sources_finished
                    );
                }
                //put it back by we dont want to recreate it
                output_cluster = Some(o_cluster);
            }
        }
    }
    pub fn get_final_duration(& mut self) -> Option<u64> {
        let mut max_duration_ns = 0;
        for source in &mut self.sources_mappings.sources {
            max_duration_ns = max_duration_ns.max(source.get_duration().unwrap_or(0));
        }
        if max_duration_ns > 0 {
            Some(max_duration_ns)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::Sink;
    use crate::source::Initialized;
    use crate::source::InputSource;
    use crate::source::Uninitialized;
    use crate::test_utils;
    use std::collections::HashMap;

    fn setup_melting_pot(source: InputSource<Uninitialized>) -> Result<(MeltingPot, u64)> {
        let (source, _cut_interval) = source.initialize(None)?;
        let timescale = source.get_target_timecode_scale()?;

        let mut mappings = SourcesMappings::new(vec![source])?;
        mappings.add_all_tracks()?;

        Ok((MeltingPot::new(mappings), timescale))
    }

    struct MockSink {
        num_clusters: usize,
        num_blocks: usize,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
                num_clusters: 0,
                num_blocks: 0,
            }
        }
    }

    impl Sink for MockSink {
        fn initialize(
            &mut self,
            _tracks: &Tracks,
            _info: &Info,
            _chapters: Option<&Chapters>,
        ) -> Result<()> {
            Ok(())
        }

        fn write_cluster(&mut self, cluster: &Cluster, _track_number: u64) -> Result<()> {
            self.num_clusters += 1;
            for block in &cluster.blocks {
                self.num_blocks += 1;
                let _ = block.track_number()?;
            }
            Ok(())
        }

        fn finalize(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_melting_pot_monotonicity() -> Result<()> {
        for source in test_utils::sources_implementations() {
            let (mut mp, timescale) = setup_melting_pot(source)?;
            let mut last_ts_per_track: HashMap<u64, i64> = HashMap::new();

            while let Some(cluster) = mp.generate_next_cluster()? {
                let cluster_ts = cluster.timestamp.0 as i64;
                for block in cluster.blocks {
                    let track_num = block.track_number()?;
                    let ts = block.timestamp_ns(cluster_ts, timescale)?;

                    if let Some(&last_ts) = last_ts_per_track.get(&track_num) {
                        assert!(
                            ts >= last_ts,
                            "Track {} timestamp regressed: {} -> {}",
                            track_num,
                            last_ts,
                            ts
                        );
                    }
                    last_ts_per_track.insert(track_num, ts);
                }
            }
            assert!(!last_ts_per_track.is_empty(), "No blocks were processed");
        }
        Ok(())
    }

    #[test]
    fn test_melting_pot_non_negative_timestamps() -> Result<()> {
        for source in test_utils::sources_implementations() {
            let (mut mp, timescale) = setup_melting_pot(source)?;
            let mut block_count = 0;

            while let Some(cluster) = mp.generate_next_cluster()? {
                let cluster_ts = cluster.timestamp.0 as i64;
                for block in cluster.blocks {
                    let track_num = block.track_number()?;
                    let ts = block.timestamp_ns(cluster_ts, timescale)?;
                    assert!(
                        ts >= 0,
                        "Track {} has negative timestamp: {}",
                        track_num,
                        ts
                    );
                    block_count += 1;
                }
            }
            assert!(block_count > 0, "No blocks were processed");
        }
        Ok(())
    }

    #[test]
    fn test_melting_pot_valid_track_numbers() -> Result<()> {
        for source in test_utils::sources_implementations() {
            let (mut mp, _) = setup_melting_pot(source)?;
            let num_mapped_tracks = mp.sources_mappings.get_current_mappings().len() as u64;
            let mut seen_tracks = HashMap::new();

            while let Some(cluster) = mp.generate_next_cluster()? {
                for block in cluster.blocks {
                    let track_num = block.track_number()?;
                    assert!(
                        track_num > 0 && track_num <= num_mapped_tracks,
                        "Invalid track number {} (max expected: {})",
                        track_num,
                        num_mapped_tracks
                    );
                    seen_tracks.insert(track_num, true);
                }
            }
            assert_eq!(
                seen_tracks.len() as u64,
                num_mapped_tracks,
                "Not all mapped tracks were seen in output"
            );
        }
        Ok(())
    }

    #[test]
    fn test_melting_pot_cluster_ordering() -> Result<()> {
        for source in test_utils::sources_implementations() {
            let (mut mp, _) = setup_melting_pot(source)?;
            let mut last_cluster_ts = 0;

            while let Some(cluster) = mp.generate_next_cluster()? {
                let current_ts = cluster.timestamp.0;
                assert!(
                    current_ts >= last_cluster_ts,
                    "Cluster timestamps are not monotonic: {} -> {}",
                    last_cluster_ts,
                    current_ts
                );
                last_cluster_ts = current_ts;
            }
        }
        Ok(())
    }

    #[test]
    fn test_melting_pot_sink_integration() -> Result<()> {
        for source in test_utils::sources_implementations() {
            let (mut mp, _) = setup_melting_pot(source)?;
            let mut mock_sink = MockSink::new();

            while let Some(cluster) = mp.generate_next_cluster()? {
                mock_sink.write_cluster(&cluster, 0)?;
            }

            assert!(mock_sink.num_clusters > 0);
            assert!(mock_sink.num_blocks > 0);
        }
        Ok(())
    }

    #[test]
    fn test_melting_pot_cluster_limits() -> Result<()> {
        use crate::cluster_warpper::CLUSTER_MAX_DURATION_NS;
        use crate::cluster_warpper::CLUSTER_MAX_SIZE_BYTES;

        let mut num_clusters = 0;
        let mut block_counter = 0;

        for source in test_utils::sources_implementations() {
            let (mut mp, timescale) = setup_melting_pot(source)?;

            while let Some(cluster) = mp.generate_next_cluster()? {
                num_clusters += 1;
                let cluster_ts = cluster.timestamp.0 as i64;
                let mut min_ts = i64::MAX;
                let mut max_ts = i64::MIN;
                let mut bytes_counter = 0;

                for block in &cluster.blocks {
                    let ts = block.timestamp_ns(cluster_ts, timescale)?;
                    min_ts = min_ts.min(ts);
                    max_ts = max_ts.max(ts);
                    bytes_counter += block.get_data()?.len() as u64;
                    block_counter += 1;
                }

                if !cluster.blocks.is_empty() {
                    let duration = (max_ts - min_ts) as u64;
                    assert!(
                        duration <= CLUSTER_MAX_DURATION_NS,
                        "Cluster duration {}ns exceeds limit {}ns",
                        duration,
                        CLUSTER_MAX_DURATION_NS
                    );
                }

                // Size check - we can't easily check encoded size without re-encoding,
                // but we know MeltingPot uses ClusterWriteWrapper which tracks it.
                // For this test, we'll just check duration as it's the most critical limit.
                assert!(
                    bytes_counter <= CLUSTER_MAX_SIZE_BYTES,
                    "Cluster size {} bytes exceeds limit {} bytes",
                    bytes_counter,
                    CLUSTER_MAX_SIZE_BYTES
                );
            }
            assert!(block_counter > 0);
            assert!(num_clusters > 0);
            assert!(
                block_counter / num_clusters > 5,
                "Too few blocks per cluster: {} / {}",
                block_counter,
                num_clusters
            );
        }
        Ok(())
    }
}
