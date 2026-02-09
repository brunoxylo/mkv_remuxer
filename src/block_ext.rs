use mkv_element::ClusterBlock;
use mkv_element::prelude::*;
use mkv_element::io::blocking_impl::*;
use std::io::Cursor;
use crate::Error;

/// Lacing mode used in a block
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LacingMode {
    /// No lacing
    None = 0b00,
    /// Xiph lacing
    Xiph = 0b01,
    /// Fixed-size lacing
    FixedSize = 0b10,
    /// EBML lacing
    Ebml = 0b11,
}

impl LacingMode {
    fn from_bits(bits: u8) -> Self {
        match bits & 0b0000_0110 >> 1 {
            0b00 => LacingMode::None,
            0b01 => LacingMode::Xiph,
            0b10 => LacingMode::FixedSize,
            0b11 => LacingMode::Ebml,
            _ => unreachable!(),
        }
    }
}

/// Helper function to get VINT length from first byte
fn vint_length(byte: u8) -> usize {
    if byte & 0x80 != 0 { 1 }
    else if byte & 0x40 != 0 { 2 }
    else if byte & 0x20 != 0 { 3 }
    else if byte & 0x10 != 0 { 4 }
    else if byte & 0x08 != 0 { 5 }
    else if byte & 0x04 != 0 { 6 }
    else if byte & 0x02 != 0 { 7 }
    else if byte & 0x01 != 0 { 8 }
    else { 0 }
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
    
    /// Get the lacing mode used in this block
    fn lacing_mode(&self) -> Option<LacingMode>;
    
    /// Check if this block is discardable
    fn is_discardable(&self) -> Result<bool, Error>;
    
    /// Get the raw flags byte
    fn flags_byte(&self) -> Result<u8, Error>;

    /// Set the track number this block belongs to
    fn set_track_number(&mut self, track_num :u64) -> Result<(), Error>;

    /// Set the relative timestamp in ticks
    fn set_timestamp(&mut self, timestamp: i16) -> Result<(), Error>;
    
    /// Set the timestamp from absolute nanoseconds
    fn set_timestamp_ns(&mut self, time_ns: i64, cluster_timestamp: i64, timecode_scale: u64) -> Result<(), Error>;
    
    /// Set the keyframe flag
    fn set_keyframe(&mut self, is_keyframe: bool) -> Result<(), Error>;
    
    /// Set the invisible flag
    fn set_invisible(&mut self, invisible: bool) -> Result<(), Error>;
    
    /// Set the discardable flag
    fn set_discardable(&mut self, discardable: bool) -> Result<(), Error>;

    // get the data of a block
    fn get_data_mut(&mut self) -> Result<&mut Vec<u8>, Error>;
    fn get_data(&self) -> Result<&Vec<u8>, Error>;
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
    
    fn lacing_mode(&self) -> Option<LacingMode> {
        let flags = self.flags_byte().ok()?;
        Some(LacingMode::from_bits(flags))
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
        data.get(track_len + 2)
            .copied()
            .ok_or_else(|| Error::InvalidBlockData(format!(
                "Cannot access flags byte at position {} in block data of length {}",
                track_len + 2,
                data.len()
            )))
    }
    fn set_track_number(&mut self, track_num: u64) -> Result<(), Error> {
        let data = self.get_data_mut()?;
        
        let track_num_vint = VInt64::new(track_num);
        let mut track_num_bytes = Vec::new();
        track_num_vint.write_to(&mut track_num_bytes).map_err(|e| Error::InvalidBlockData(format!("Failed to write track number: {}", e)))?;

        let old_track_len = vint_length(data[0]);
        let new_track_len = track_num_bytes.len();

        if new_track_len != old_track_len {
            // Size mismatch - need to reconstruct the entire block
            let mut new_data = track_num_bytes.clone();
            new_data.extend_from_slice(&data[old_track_len..]);
            *data = new_data;
        } else {
            // Same size - safe to overwrite in place
            data[0..new_track_len].copy_from_slice(&track_num_bytes);
        }
        
        Ok(())
    }

    fn set_timestamp(&mut self, timestamp: i16) -> Result<(), Error> {
        let data = self.get_data_mut()?;
        
        let track_len = vint_length(data[0]);
        let bytes = timestamp.to_be_bytes();
        data[track_len] = bytes[0];
        data[track_len + 1] = bytes[1];
        Ok(())
    }
    
    fn set_timestamp_ns(&mut self, time_ns: i64, cluster_timestamp: i64, timecode_scale: u64) -> Result<(), Error> {
        let new_ticks = time_ns / timecode_scale as i64;
        let new_rel_ticks = new_ticks - cluster_timestamp;
        let clamped = new_rel_ticks.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        self.set_timestamp(clamped)
    }
    
    fn set_keyframe(&mut self, is_keyframe: bool) -> Result<(), Error> {
        let data = self.get_data_mut()?;
        
        let track_len = vint_length(data[0]);
        if is_keyframe {
            data[track_len + 2] |= 0x80;  // Set bit 7
        } else {
            data[track_len + 2] &= !0x80; // Clear bit 7
        }
        Ok(())
    }
    
    fn set_invisible(&mut self, invisible: bool) -> Result<(), Error> {
        let data = self.get_data_mut()?;
        
        let track_len = vint_length(data[0]);
        if invisible {
            data[track_len + 2] |= 0x08;  // Set bit 3
        } else {
            data[track_len + 2] &= !0x08; // Clear bit 3
        }
        Ok(())
    }
    
    fn set_discardable(&mut self, discardable: bool) -> Result<(), Error> {
        let data = self.get_data_mut()?;
        
        let track_len = vint_length(data[0]);
        if discardable {
            data[track_len + 2] |= 0x01;  // Set bit 0
        } else {
            data[track_len + 2] &= !0x01; // Clear bit 0
        }
        Ok(())
    }
    fn get_data_mut(&mut self) -> Result<&mut Vec<u8>, Error> {
        let data = match self {
            ClusterBlock::Simple(sb) => &mut sb.0,
            ClusterBlock::Group(bg) => &mut bg.block.0,
        };
        if data.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        Ok(data)
    }
    fn get_data(&self) -> Result<&Vec<u8>, Error> {
        let data = match self {
            ClusterBlock::Simple(sb) => &sb.0,
            ClusterBlock::Group(bg) => &bg.block.0,
        };
        if data.len() < 4 {
            return Err(Error::InvalidBlockData("Block data too short".to_string()));
        }
        Ok(data)
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
            16 => TrackKind::Logo   ,
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
}

impl TracksExt for Tracks {
    fn get_track_kind(&self, track_number: u64) -> Option<TrackKind> {
        self.track_entry.iter()
            .find(|te| te.track_number.0 == track_number)
            .map(|te| TrackKind::from_u64(te.track_type.0))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lacing_mode_from_bits() {
        assert_eq!(LacingMode::from_bits(0b0000_0000), LacingMode::None);
        assert_eq!(LacingMode::from_bits(0b0000_0010), LacingMode::Xiph);
        assert_eq!(LacingMode::from_bits(0b0000_0100), LacingMode::FixedSize);
        assert_eq!(LacingMode::from_bits(0b0000_0110), LacingMode::Ebml);
    }
}
