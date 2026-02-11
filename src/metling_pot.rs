use crate::Error;
use crate::{
    Cluster, ClusterBlockExt, ClusterReadWrapper, ClusterWriteWrapper, Result, SourcesMappings,
};
use log::{debug, warn};
use mkv_element::prelude::*;

pub struct MeltingPot {
    sources_mappings: SourcesMappings,
    clusters: Vec<Option<ClusterReadWrapper>>,
}

impl MeltingPot {
    pub fn new(sources_mappings: SourcesMappings) -> Self {
        let num_sources = sources_mappings.sources.len();
        // Initialize empty vector and fill with None
        let initial_clusters = (0..num_sources).map(|_| None).collect();
        Self {
            sources_mappings,
            clusters: initial_clusters,
        }
    }
    pub fn generate_next_cluster(&mut self) -> Result<Option<Cluster>> {
        let timescale = self.sources_mappings.get_time_scale()?;
        let mut output_cluster = ClusterWriteWrapper::new(0, timescale);
        let mut iteration = 0;
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
            if all_sources_finished || output_cluster.is_limit_reached() {
                if output_cluster.is_empty() {
                    debug!("No more clusters available");
                    return Ok(None);
                } else {
                    return Ok(Some(output_cluster.finish()));
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
                            // yes we are writing into the input clusters buffer, but this is fine since we will never read from it again and it saves us from having to clone the block
                            block.set_track_number(output_trackindex)?;
                            output_cluster.add_block(&block, lowest_timestamp_ns)?
                        };
                    } else {
                        // No more blocks in this cluster, set to None to fetch next cluster from source in next iteration
                        self.clusters[lowest_index] = None;
                    }
                }
            } else {
                // No valid block found but sources aren't finished - this shouldn't happen
                warn!(
                    "No valid block found in iteration {}, all_sources_finished={}",
                    iteration, all_sources_finished
                );
            }
        }
    }
}
