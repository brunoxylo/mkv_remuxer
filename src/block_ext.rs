use crate::Error;
use bytes::{Bytes, BytesMut};
use log::trace;
use mkv_element::ClusterBlock;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::io::{Cursor, Read, Seek, SeekFrom};

/// Returns a mutable reference to the raw block bytes inside a `ClusterBlock`.
/// Used by all `set_*` methods so they can write the modified `BytesMut` back.
fn block_raw_data_mut(block: &mut ClusterBlock) -> &mut Bytes {
    match block {
        ClusterBlock::Simple(sb) => &mut sb.0,
        ClusterBlock::Group(bg) => &mut bg.block.0,
    }
}

/// Helper function to get VINT length from first byte
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
    } else if byte & 0x01 != 0 {
        8
    } else {
        0
    }
}

/// Extension trait for ClusterBlock providing access to block header fields
/// We don't use the frame struct form the mkv_element bc it does not allow us top set flags back into the original block.
pub trait ClusterBlockExt {
    /// Get the track number this block belongs to
    fn track_number(&self) -> Result<u64, Error>;

    /// Get the relative timestamp in ticks (relative to cluster timestamp)
    fn timestamp(&self) -> Result<i16, Error>;

    /// Get the absolute timestamp in nanoseconds given the cluster timestamp and timecode scale
    fn timestamp_ns(&self, cluster_timestamp: i64, timecode_scale: u64) -> Result<i64, Error>;

    /// Check if this is a keyframe (I-frame/sync point)
    fn is_keyframe(&self) -> Result<bool, Error>;

    /// Check if the invisible flag is set (codec should decode but not display)
    fn is_invisible(&self) -> Result<bool, Error>;

    /// Check if this block is discardable
    fn is_discardable(&self) -> Result<bool, Error>;

    /// Get the raw flags byte
    fn flags_byte(&self) -> Result<u8, Error>;

    /// Set the track number this block belongs to
    fn set_track_number(&mut self, track_num: u64) -> Result<(), Error>;

    /// Set the relative timestamp in ticks
    fn set_timestamp(&mut self, timestamp: i16) -> Result<(), Error>;

    /// Set the timestamp from absolute nanoseconds
    fn set_timestamp_ns(
        &mut self,
        time_ns: u64,
        cluster_timestamp: u64,
        timecode_scale: u64,
    ) -> Result<(), Error>;

    /// Set the keyframe flag
    fn set_keyframe(&mut self, is_keyframe: bool) -> Result<(), Error>;

    /// Set the invisible flag
    fn set_invisible(&mut self, invisible: bool) -> Result<(), Error>;

    /// Set the discardable flag
    fn set_discardable(&mut self, discardable: bool) -> Result<(), Error>;

    // get the data of a block
    fn get_data_mut(&mut self) -> Result<BytesMut, Error>;
    fn get_data(&self) -> Result<Bytes, Error>;

    /// Get the block duration in Track Ticks
    /// Returns None if:
    /// - This is a SimpleBlock (which doesn't support BlockDuration)
    /// - This is a BlockGroup but BlockDuration is not set
    fn get_block_duration(&self) -> Option<u64>;

    /// Set the block duration in Track Ticks
    ///
    /// # Arguments
    /// * `duration` - Duration in Track Ticks, or None to remove the duration field
    ///
    /// # Returns
    /// * `Err` if this is a SimpleBlock (which doesn't support BlockDuration)
    /// * `Ok(())` if this is a BlockGroup (even if duration is None)
    fn set_block_duration(&mut self, duration: Option<u64>) -> Result<(), Error>;
}

pub trait ClusterExt {
    fn get_timestamp_ns(&self, timecode_scale: u64) -> u64;
    fn set_timestamp_ns(&mut self, timestamp_ns: u64, timecode_scale: u64);
    /// return a list of indexes of keyframe blocks in the clusters blocks array
    fn get_keyframes(&self, track_num: u64) -> Vec<usize>;
    fn has_keyframes(&self, track_num: u64) -> bool {
        !self.get_keyframes(track_num).is_empty()
    }
    /// Get the index of the last keyframe block inside the cluster's block array before the given timestamp (in nanoseconds)
    fn get_keyframe_before(
        &self,
        track_num: u64,
        timestamp_ns: i64,
        timecode_scale: u64,
    ) -> Option<usize>;
    /// Get the index of the first keyframe block inside the cluster's block array after the given timestamp (in nanoseconds)
    fn get_keyframe_after(
        &self,
        track_num: u64,
        timestamp_ns: i64,
        timecode_scale: u64,
    ) -> Option<usize>;
    fn from_file_pos(file: &mut (impl Read + Seek), file_pos: u64) -> Result<Cluster, Error>;
}

impl ClusterExt for Cluster {
    fn get_timestamp_ns(&self, timecode_scale: u64) -> u64 {
        self.timestamp.0 * timecode_scale
    }
    fn set_timestamp_ns(&mut self, timestamp_ns: u64, timecode_scale: u64) {
        self.timestamp.0 = timestamp_ns / timecode_scale;
    }
    fn get_keyframes(&self, track_num: u64) -> Vec<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter_map(|(i, block)| {
                if let Ok(true) = block.is_keyframe() {
                    if let Ok(block_track_num) = block.track_number() {
                        if block_track_num == track_num {
                            return Some(i);
                        }
                    }
                }
                None
            })
            .collect()
    }
    fn get_keyframe_before(
        &self,
        track_num: u64,
        timestamp_ns: i64,
        timecode_scale: u64,
    ) -> Option<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter_map(|(i, block)| {
                if let Ok(true) = block.is_keyframe() {
                    if let Ok(block_track_num) = block.track_number() {
                        if block_track_num == track_num {
                            // is block for the right track?
                            if let Ok(block_ts_ns) =
                                block.timestamp_ns(self.timestamp.0 as i64, timecode_scale)
                            {
                                trace!(
                                    "Block {}: timestamp {} ns, reference {}",
                                    i, block_ts_ns, timestamp_ns
                                );
                                if block_ts_ns <= timestamp_ns {
                                    return Some((i, block_ts_ns));
                                }
                            }
                        }
                    }
                }
                None
            })
            .max_by_key(|&(_, ts)| ts)
            .map(|(i, _)| i)
    }
    fn get_keyframe_after(
        &self,
        track_num: u64,
        timestamp_ns: i64,
        timecode_scale: u64,
    ) -> Option<usize> {
        self.blocks.iter().enumerate().find_map(|(i, block)| {
            if let Ok(true) = block.is_keyframe() {
                if let Ok(block_track_num) = block.track_number() {
                    if block_track_num == track_num {
                        if let Ok(block_ts_ns) =
                            block.timestamp_ns(self.timestamp.0 as i64, timecode_scale)
                        {
                            if block_ts_ns >= timestamp_ns {
                                return Some(i);
                            }
                        }
                    }
                }
            }
            None
        })
    }
    /// Read a Cluster from a file at the given position, preserving the initial file position after reading
    fn from_file_pos(file: &mut (impl Read + Seek), file_pos: u64) -> Result<Self, Error> {
        let old_pos = file.stream_position()?;
        file.seek(SeekFrom::Start(file_pos))?;
        let header = match Header::read_from(file) {
            Ok(h) => h,
            Err(e) => {
                return Err(Error::InvalidFilePos(format!(
                    "Cluster header could not be read - wrong position? {}",
                    e
                )));
            }
        };

        if header.id == Cluster::ID {
            let cluster = Cluster::read_element(&header, file);
            file.seek(SeekFrom::Start(old_pos))?;
            match cluster {
                Ok(c) => return Ok(c),
                Err(e) => {
                    return Err(Error::InvalidFilePos(format!(
                        "Cluster element could not be read - wrong position? {}",
                        e
                    )));
                }
            };
        } else {
            file.seek(SeekFrom::Start(old_pos))?;
            return Err(Error::InvalidBlockData("Not a cluster".to_string()));
        }
    }
}

impl ClusterBlockExt for ClusterBlock {
    fn track_number(&self) -> Result<u64, Error> {
        let data = self.get_data()?;

        let mut cursor = Cursor::new(&data[..]);
        VInt64::read_from(&mut cursor)
            .map(|v| v.value)
            .map_err(|e| Error::InvalidBlockData(format!("Failed to read track number: {}", e)))
    }

    fn timestamp(&self) -> Result<i16, Error> {
        let data = self.get_data()?;

        let track_len = vint_length(data[0]);
        let tc_bytes = [data[track_len], data[track_len + 1]];
        Ok(i16::from_be_bytes(tc_bytes))
    }

    fn timestamp_ns(&self, cluster_timestamp: i64, timecode_scale: u64) -> Result<i64, Error> {
        let rel_ticks = self.timestamp()?;
        let abs_ticks = cluster_timestamp + rel_ticks as i64;
        Ok(abs_ticks * timecode_scale as i64)
    }

    fn is_keyframe(&self) -> Result<bool, Error> {
        match self {
            ClusterBlock::Simple(_) => {
                let flags = self.flags_byte()?;
                // Keyframe flag is bit 7 (0x80)
                Ok((flags & 0x80) != 0)
            }
            ClusterBlock::Group(bg) => {
                // For BlockGroup, check if ReferenceBlock is absent (means keyframe)
                Ok(bg.reference_block.is_empty())
            }
        }
    }

    fn is_invisible(&self) -> Result<bool, Error> {
        let flags = self.flags_byte()?;
        // Invisible flag is bit 3 (0x08)
        Ok((flags & 0x08) != 0)
    }

    fn is_discardable(&self) -> Result<bool, Error> {
        match self {
            ClusterBlock::Simple(_) => {
                let flags = self.flags_byte()?;
                // Discardable flag is bit 0 (0x01)
                Ok((flags & 0x01) != 0)
            }
            ClusterBlock::Group(bg) => {
                // For BlockGroup, check if DiscardPadding element exists
                // A block is discardable if it has a discard padding element
                Ok(bg.discard_padding.is_some())
            }
        }
    }

    fn flags_byte(&self) -> Result<u8, Error> {
        let data = self.get_data()?;

        let track_len = vint_length(data[0]);
        // Flags byte is at track_len + 2 (after track number and 2-byte timestamp)
        data.get(track_len + 2).copied().ok_or_else(|| {
            Error::InvalidBlockData(format!(
                "Cannot access flags byte at position {} in block data of length {}",
                track_len + 2,
                data.len()
            ))
        })
    }
    fn set_track_number(&mut self, track_num: u64) -> Result<(), Error> {
        let data_ref = block_raw_data_mut(self);
        if data_ref.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }

        let track_num_vint = VInt64::new(track_num);
        let mut track_num_bytes = Vec::new();
        track_num_vint
            .write_to(&mut track_num_bytes)
            .map_err(|e| Error::InvalidBlockData(format!("Failed to write track number: {}", e)))?;

        let old_track_len = vint_length(data_ref[0]);
        let new_track_len = track_num_bytes.len();

        let mut data = BytesMut::from(data_ref.clone());
        if new_track_len != old_track_len {
            let tail = data[old_track_len..].to_vec();
            data.clear();
            data.extend_from_slice(&track_num_bytes);
            data.extend_from_slice(&tail);
        } else {
            data[0..new_track_len].copy_from_slice(&track_num_bytes);
        }
        *data_ref = data.freeze();

        Ok(())
    }

    fn set_timestamp(&mut self, timestamp: i16) -> Result<(), Error> {
        let data_ref = block_raw_data_mut(self);
        if data_ref.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        let track_len = vint_length(data_ref[0]);
        let mut data = BytesMut::from(data_ref.clone());
        let bytes = timestamp.to_be_bytes();
        data[track_len] = bytes[0];
        data[track_len + 1] = bytes[1];
        *data_ref = data.freeze();
        Ok(())
    }

    fn set_timestamp_ns(
        &mut self,
        time_ns: u64,
        cluster_timestamp: u64,
        timecode_scale: u64,
    ) -> Result<(), Error> {
        let new_ticks = time_ns / timecode_scale;
        let new_rel_ticks = new_ticks as i64 - cluster_timestamp as i64;
        let clamped = new_rel_ticks.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        self.set_timestamp(clamped)
    }

    fn set_keyframe(&mut self, is_keyframe: bool) -> Result<(), Error> {
        let data_ref = block_raw_data_mut(self);
        if data_ref.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        let track_len = vint_length(data_ref[0]);
        let mut data = BytesMut::from(data_ref.clone());
        if is_keyframe {
            data[track_len + 2] |= 0x80;
        } else {
            data[track_len + 2] &= !0x80;
        }
        *data_ref = data.freeze();
        Ok(())
    }

    fn set_invisible(&mut self, invisible: bool) -> Result<(), Error> {
        let data_ref = block_raw_data_mut(self);
        if data_ref.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        let track_len = vint_length(data_ref[0]);
        let mut data = BytesMut::from(data_ref.clone());
        if invisible {
            data[track_len + 2] |= 0x08;
        } else {
            data[track_len + 2] &= !0x08;
        }
        *data_ref = data.freeze();
        Ok(())
    }

    fn set_discardable(&mut self, discardable: bool) -> Result<(), Error> {
        let data_ref = block_raw_data_mut(self);
        if data_ref.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        let track_len = vint_length(data_ref[0]);
        let mut data = BytesMut::from(data_ref.clone());
        if discardable {
            data[track_len + 2] |= 0x01;
        } else {
            data[track_len + 2] &= !0x01;
        }
        *data_ref = data.freeze();
        Ok(())
    }
    fn get_data_mut(&mut self) -> Result<BytesMut, Error> {
        let data = block_raw_data_mut(self);
        if data.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        Ok(BytesMut::from(data.clone()))
    }
    fn get_data(&self) -> Result<Bytes, Error> {
        let data = match self {
            ClusterBlock::Simple(sb) => &sb.0,
            ClusterBlock::Group(bg) => &bg.block.0,
        };
        if data.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        Ok(data.clone())
    }

    fn get_block_duration(&self) -> Option<u64> {
        match self {
            ClusterBlock::Simple(_) => None, // SimpleBlock doesn't support BlockDuration
            ClusterBlock::Group(bg) => bg.block_duration.as_ref().map(|bd| bd.0),
        }
    }

    fn set_block_duration(&mut self, duration: Option<u64>) -> Result<(), Error> {
        match self {
            ClusterBlock::Simple(_) => Err(Error::InvalidBlockData(
                "Cannot set BlockDuration on SimpleBlock".to_string(),
            )),
            ClusterBlock::Group(bg) => {
                bg.block_duration = duration.map(BlockDuration);
                Ok(())
            }
        }
    }
}

/// Track kind/type enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// Video track
    Video = 1,
    /// Audio track
    Audio = 2,
    /// Complex track
    Complex = 3,
    /// Logo track
    Logo = 16,
    /// Subtitle track
    Subtitle = 17,
    /// Buttons track
    Buttons = 18,
    /// Control track
    Control = 32,
    /// Metadata track
    Metadata = 33,
}

impl TrackKind {
    /// Convert from u64 value to TrackKind
    pub fn from_u64(value: u64) -> Self {
        match value {
            1 => TrackKind::Video,
            2 => TrackKind::Audio,
            3 => TrackKind::Complex,
            16 => TrackKind::Logo,
            17 => TrackKind::Subtitle,
            18 => TrackKind::Buttons,
            32 => TrackKind::Control,
            _ => TrackKind::Metadata, // Treat unknown types as Metadata
        }
    }
}

impl PartialEq<u64> for TrackKind {
    fn eq(&self, other: &u64) -> bool {
        *self as u64 == *other
    }
}

impl PartialEq<TrackKind> for u64 {
    fn eq(&self, other: &TrackKind) -> bool {
        *self == *other as u64
    }
}

pub trait TracksExt {
    fn get_track_kind(&self, track_number: u64) -> Option<TrackKind>;
    /// Get a list of all track numbers that match the given track kind. If track_kind is None, return all track numbers regardless of kind.
    fn get_all_track_numbers(&self, track_kind: Option<TrackKind>) -> Vec<u64>;
    /// Get a list of all video track numbers
    fn get_all_video_tracks(&self) -> Vec<u64> {
        self.get_all_track_numbers(Some(TrackKind::Video))
    }
    /// Get a list of all audio track numbers
    fn get_all_audio_tracks(&self) -> Vec<u64> {
        self.get_all_track_numbers(Some(TrackKind::Audio))
    }
    /// Get a list of all subtitle track numbers
    fn get_all_subtitle_tracks(&self) -> Vec<u64> {
        self.get_all_track_numbers(Some(TrackKind::Subtitle))
    }
}

impl TracksExt for Tracks {
    fn get_track_kind(&self, track_number: u64) -> Option<TrackKind> {
        self.track_entry
            .iter()
            .find(|te| te.track_number.0 == track_number)
            .map(|te| TrackKind::from_u64(te.track_type.0))
    }
    fn get_all_track_numbers(&self, track_kind: Option<TrackKind>) -> Vec<u64> {
        self.track_entry
            .iter()
            .filter_map(|te| {
                let kind = TrackKind::from_u64(te.track_type.0);
                if track_kind.is_none() || track_kind.unwrap() == kind {
                    Some(te.track_number.0)
                } else {
                    None
                }
            })
            .collect()
    }
}
