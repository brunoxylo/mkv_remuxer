use mkv_element::prelude::{TrackEntry, Tracks};

use crate::{Error, Result, block_ext::TrackKind, source::{Remuxing, InputSource}};

/// Wrapper struct to hold multiple sources and their track mappings for remuxing
/// provides convenient methods for managing track mappings
/// note the order of added mappings determines the order of tracks in the output file (e.g. if you want to prioritize video tracks from the first source, add those mappings first)
pub struct SourcesMappings {
    pub sources: Vec<InputSource<Remuxing>>,
    mappings: Vec<(u64, u64, TrackKind)>, // (input file index, track number, track kind)
}

impl SourcesMappings {
    pub fn new(sources: Vec<InputSource<Remuxing>>) -> Result<Self> {
        let first_output_timescale = if let Some(first_source) = sources.first() {
            first_source.get_target_timecode_scale()?
        } else {
            return Err(Error::MissingElement("No sources provided".to_string()));
        };
        for source in &sources {
            let source_timescale = source.get_target_timecode_scale()?;
            if source_timescale != first_output_timescale {
                return Err(Error::TimecodeScaleError(format!(
                    "Source timescale {} does not match output timescale {}. Please set the target timecode scale for all sources to the same value.",
                    source_timescale, first_output_timescale
                )));
            }
        }
        Ok(Self {
            sources,
            mappings: Vec::new(),
        })
    }

    pub fn get_current_mappings(&self) -> &Vec<(u64, u64, TrackKind)> {
        &self.mappings
    }
    pub fn delete_current_mappings(&mut self) {
        self.mappings.clear();
    }  

    /// Get all tracks from all sources as 2D array, first index is the source index, second index is the track number
    pub fn get_all_input_tracks(&mut self) -> Result<Vec<Vec<TrackEntry>>> {
        let mut all_tracks = Vec::new();
        for (_index, source) in self.sources.iter_mut().enumerate() {
            let tracks = source.get_tracks()?;
            all_tracks.push(tracks.track_entry)
        }
        Ok(all_tracks)
    }
    /// check whether the specified track is actually mapped by the current mappings
    /// if mapped it return the track number in the output file (which is the index in the mappings vector), otherwise returns None
    pub fn is_track_mapped(&self, source_index: u64, track_number: u64) -> Option<u64> {
        self.mappings.iter().position(|(s_idx, t_num, _)| *s_idx == source_index && *t_num == track_number).map(|idx| idx as u64 +1 ) // output mkv track numbers are 1-based
    }

    pub fn get_track_kind(&self, source_index: u64, track_number: u64) -> Result<TrackKind> {
        self.mappings
            .iter()
            .find(|(s_idx, t_num, _)| *s_idx == source_index && *t_num == track_number)
            .map(|(_, _, kind)| *kind)
            .ok_or_else(|| Error::TrackMappingError(format!(
                "Track number {} not found in source {}",
                track_number, source_index
            )))
    }

    
    /// Add a specific track from a specific source to the mappings
    /// NOTE: source index starts from zero but track number is 1 based as stored in MKV files
    pub fn add_mapping(&mut self, source_index: u64, track_number: u64) -> Result<()> {
        // check if source index is valid
        if source_index as usize >= self.sources.len() {
            return Err(Error::TrackMappingError(format!(
                "Invalid source index {}",
                source_index
            )));
        }
        // check if track number exists in the source and get its kind
        let tracks = self.sources[source_index as usize].get_tracks()?;
        let track_kind = tracks
            .track_entry
            .iter()
            .find(|t| t.track_number.0 == track_number)
            .map(|t| TrackKind::from_u64(t.track_type.0))
            .ok_or_else(|| Error::TrackMappingError(format!(
                "Track number {} not found in source {}",
                track_number, source_index
            )))?;
        self.mappings.push((source_index, track_number, track_kind));
        Ok(())
    }
    /// Add the first video track found in the the sources to the mappings
    pub fn add_first_video_track(&mut self) -> Result<()> {
        for (index, source) in self.sources.iter_mut().enumerate() {
            let tracks = source.get_tracks()?;
            for track in tracks.track_entry.iter() {
                if track.track_type.0 == TrackKind::Video { // Video track type
                    self.mappings.push((index as u64, track.track_number.0, TrackKind::from_u64(track.track_type.0)));
                    return Ok(());
                }
            }
        }
        Err(Error::NoTracksOfType("video".to_string()))
    }
    // add all tracks
    pub fn add_all_tracks(&mut self) -> Result<()> {
        self.add_tracks_by_type(None)
    }
    /// Add all video tracks from all sources to the mappings
    pub fn add_all_video_tracks(&mut self) -> Result<()> {
        self.add_tracks_by_type(Some(TrackKind::Video)) // Video track type
    }
    /// Add all audio tracks from all sources to the mappings
    pub fn add_all_audio_tracks(&mut self) -> Result<()> {
        self.add_tracks_by_type(Some(TrackKind::Audio)) // Audio track type
    }
    // Add all subtitle tracks from all sources to the mappings
    pub fn add_all_subtitle_tracks(&mut self) -> Result<()> {
        self.add_tracks_by_type(Some(TrackKind::Subtitle)) // Subtitle track type
    }
    // add audio tracks with specific language code (e.g. "eng") to the mappings
    pub fn add_audio_tracks_by_language(&mut self, language_code: &str) -> Result<()> {
        for (index, source) in self.sources.iter_mut().enumerate() {
            let tracks = source.get_tracks()?;
            for track in tracks.track_entry.iter() {
                if track.track_type.0 == TrackKind::Audio { // Audio track type
                    if track.language.0 == language_code {
                        self.mappings.push((index as u64, track.track_number.0, TrackKind::from_u64(track.track_type.0)));
                    }
                }
            }
        }
        Ok(())    
    }
    
    /// Add all tracks from all sources to the mappings
    fn add_tracks_by_type(&mut self, track_type: Option<TrackKind>) -> Result<()> {
        for (index, source) in self.sources.iter_mut().enumerate() {
            let tracks = source.get_tracks()?;
            for track in tracks.track_entry.iter() {
                if let Some(tt) = track_type {
                    if track.track_type.0 == tt {
                        self.mappings.push((index as u64, track.track_number.0, TrackKind::from_u64(track.track_type.0)));
                    }
                } else {
                    self.mappings.push((index as u64, track.track_number.0, TrackKind::from_u64(track.track_type.0)));
                }
            }
        }
        Ok(())
    }
    pub fn get_time_scale(&self) -> Result<u64> {
        if let Some(first_source) = self.sources.first() {
            first_source.get_target_timecode_scale()
        } else {
            Err(Error::MissingElement("No sources provided".to_string()))
        }
    }

    pub fn get_output_tracks_metadata(&self) -> Result<Tracks> {
        let mut output_tracks = Vec::new();
        for (output_index, (source_index, source_track_number, _)) in self.mappings.iter().enumerate() {
            let source = &self.sources[*source_index as usize];
            let tracks = source.get_tracks()?;
            if let Some(track) = tracks.track_entry.iter().find(|t| t.track_number.0 == *source_track_number) {
                let mut output_track = track.clone();
                // Set the track number to match the output position (1-based)
                output_track.track_number.0 = (output_index + 1) as u64;
                output_tracks.push(output_track);
            } else {
                return Err(Error::TrackMappingError(format!(
                    "Track number {} not found in source {}",
                    source_track_number, source_index
                )));
            }
        }
        Ok(Tracks { 
            track_entry: output_tracks,
            crc32: None,
            void: None,
        })
    }

}