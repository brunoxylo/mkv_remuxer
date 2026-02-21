//! Simplified AV1 frame header parser for reference frame tracking
//!
//! This module provides minimal parsing of AV1 OBU (Open Bitstream Unit) structures
//! to extract reference frame information needed for pre-roll frame calculation.

use std::io;

/// AV1 frame types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    KeyFrame = 0,
    InterFrame = 1,
    IntraOnlyFrame = 2,
    SwitchFrame = 3,
}

/// Parsed AV1 frame header with reference frame information
#[derive(Debug, Clone)]
pub struct Av1FrameHeader {
    /// Frame type (KEY_FRAME, INTER_FRAME, INTRA_ONLY_FRAME, SWITCH_FRAME)
    pub frame_type: FrameType,
    
    /// 8-bit bitfield indicating which reference frame slots this frame updates
    /// Bit i set means this frame will be stored in slot i after decoding
    pub refresh_frame_flags: u8,
    
    /// Indices into the 8 reference frame buffer slots that this frame reads from
    /// Only valid for INTER_FRAME and SWITCH_FRAME types
    /// Array of 7 values mapping LAST_FRAME through ALTREF_FRAME to buffer slots
    pub ref_frame_idx: [u8; 7],
    
    /// Whether this frame shows an existing reference frame (3-byte OBU)
    pub show_existing_frame: bool,
    
    /// If show_existing_frame is true, which slot to display
    pub frame_to_show_map_idx: u8,
}

impl Av1FrameHeader {
    /// Get which reference frame slots this frame updates (writes to)
    ///
    /// Returns a Vec of slot indices (0-7) where bits are set in refresh_frame_flags
    pub fn get_updated_slots(&self) -> Vec<u8> {
        (0..8)
            .filter(|i| (self.refresh_frame_flags & (1 << i)) != 0)
            .collect()
    }
    
    /// Get which reference frame slots this frame reads from (dependencies)
    ///
    /// Only valid for INTER_FRAME and SWITCH_FRAME types
    pub fn get_dependency_slots(&self) -> &[u8; 7] {
        &self.ref_frame_idx
    }
    
    /// Check if this frame updates a specific slot
    pub fn updates_slot(&self, slot: u8) -> bool {
        if slot >= 8 {
            return false;
        }
        (self.refresh_frame_flags & (1 << slot)) != 0
    }
    
    /// Parse AV1 frame header from raw frame data
    ///
    /// This is a simplified parser that extracts only the reference frame information
    /// needed for pre-roll calculation. It does not perform full AV1 decoding.
    pub fn parse(_data: &[u8]) -> io::Result<Self> {
        // TODO: Implement actual AV1 OBU parsing
        // This requires:
        // 1. Parse OBU header (type, has_size_field, extension, etc.)
        // 2. Handle variable-length LEB128 size encoding
        // 3. Bit-level parsing of frame header
        // 4. Conditional field parsing based on frame_type
        
        // Placeholder implementation
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "AV1 frame header parsing not yet implemented"
        ))
    }
}

/// OBU (Open Bitstream Unit) types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum ObuType {
    SequenceHeader = 1,
    TemporalDelimiter = 2,
    FrameHeader = 3,
    TileGroup = 4,
    Metadata = 5,
    Frame = 6,
    RedundantFrameHeader = 7,
    TileList = 8,
    Padding = 15,
}

/// Simple bitstream reader for bit-level parsing
struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7, position within current byte
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }
    
    /// Read n bits (up to 32) as u32
    fn read_bits(&mut self, n: u8) -> io::Result<u32> {
        if n > 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot read more than 32 bits at once"
            ));
        }
        
        let mut result = 0u32;
        let mut bits_remaining = n;
        
        while bits_remaining > 0 {
            if self.byte_pos >= self.data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Not enough data for bit read"
                ));
            }
            
            let bits_in_current_byte = 8 - self.bit_pos;
            let bits_to_read = bits_remaining.min(bits_in_current_byte);
            
            let byte = self.data[self.byte_pos];
            let shift = bits_in_current_byte - bits_to_read;
            let mask = ((1u8 << bits_to_read) - 1) << shift;
            let bits = ((byte & mask) >> shift) as u32;
            
            result = (result << bits_to_read) | bits;
            
            self.bit_pos += bits_to_read;
            if self.bit_pos >= 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
            
            bits_remaining -= bits_to_read;
        }
        
        Ok(result)
    }
    
    /// Read a boolean (1 bit)
    fn read_bool(&mut self) -> io::Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_bit_reader() {
        // Test data: 0b10110011 0b11001010
        let data = [0b10110011, 0b11001010];
        let mut reader = BitReader::new(&data);
        
        assert_eq!(reader.read_bits(1).unwrap(), 1); // 1
        assert_eq!(reader.read_bits(2).unwrap(), 0b01); // 01
        assert_eq!(reader.read_bits(3).unwrap(), 0b100); // 100
        assert_eq!(reader.read_bits(4).unwrap(), 0b1111); // 1100 (from first byte) + 11 (from second)
    }
    
    #[test]
    fn test_updated_slots() {
        let header = Av1FrameHeader {
            frame_type: FrameType::InterFrame,
            refresh_frame_flags: 0b00000101, // slots 0 and 2
            ref_frame_idx: [7, 6, 5, 4, 3, 2, 1],
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
        };
        
        let slots = header.get_updated_slots();
        assert_eq!(slots, vec![0, 2]);
    }
    
    #[test]
    fn test_updates_slot() {
        let header = Av1FrameHeader {
            frame_type: FrameType::KeyFrame,
            refresh_frame_flags: 0xFF, // all slots
            ref_frame_idx: [0; 7],
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
        };
        
        for i in 0..8 {
            assert!(header.updates_slot(i));
        }
    }
}
