//! Simplified VP9 frame header parser for reference frame tracking
//!
//! This module provides minimal parsing of VP9 frame headers to extract
//! reference frame information needed for pre-roll frame calculation.

use std::io;

/// VP9 frame types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vp9FrameType {
    KeyFrame = 0,
    InterFrame = 1,
}

/// VP9 reference frame types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Vp9RefFrame {
    Intra = 0,
    LastFrame = 1,
    GoldenFrame = 2,
    AltRefFrame = 3,
    Last2Frame = 4,
    Last3Frame = 5,
    Golden2Frame = 6,
    AltRef2Frame = 7,
}

/// Parsed VP9 frame header with reference frame information
#[derive(Debug, Clone)]
pub struct Vp9FrameHeader {
    /// Frame type (KEY_FRAME, INTER_FRAME)
    pub frame_type: Vp9FrameType,
    
    /// Whether this is an intra-only frame
    /// Intra-only frames are like keyframes but can appear in inter-frame sequences
    pub intra_only: bool,
    
    /// 8-bit bitfield indicating which reference frame slots this frame updates
    /// Bit i set means this frame will be stored in slot i after decoding
    pub refresh_frame_flags: u8,
    
    /// Which reference frames this frame uses (for INTER frames)
    /// VP9 can use up to 3 reference frames per inter frame
    pub ref_frame_idx: [u8; 3],
    
    /// Whether to show an existing frame instead of decoding
    pub show_existing_frame: bool,
    
    /// If show_existing_frame is true, which frame to show
    pub frame_to_show_map_idx: u8,
}

impl Vp9FrameHeader {
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
    /// Only valid for INTER_FRAME types
    pub fn get_dependency_slots(&self) -> &[u8; 3] {
        &self.ref_frame_idx
    }
    
    /// Check if this frame updates a specific slot
    pub fn updates_slot(&self, slot: u8) -> bool {
        if slot >= 8 {
            return false;
        }
        (self.refresh_frame_flags & (1 << slot)) != 0
    }
    
    /// Check if this is a clean random access point
    ///
    /// VP9 keyframes typically refresh all reference buffers, making them
    /// better random access points than AV1 keyframes
    pub fn is_random_access_point(&self) -> bool {
        match self.frame_type {
            Vp9FrameType::KeyFrame => true,
            Vp9FrameType::InterFrame => self.intra_only && self.refresh_frame_flags == 0xFF,
        }
    }
    
    /// Parse VP9 frame header from raw frame data
    ///
    /// This is a simplified parser that extracts only the reference frame information
    /// needed for pre-roll calculation. It does not perform full VP9 decoding.
    pub fn parse(_data: &[u8]) -> io::Result<Self> {
        // TODO: Implement actual VP9 frame header parsing
        // This requires:
        // 1. Parse uncompressed header (frame marker, profile, etc.)
        // 2. Read frame_type (1 bit)
        // 3. Read show_frame (1 bit)
        // 4. Read error_resilient_mode (1 bit)
        // 5. For KEY_FRAME: parse frame sync code, color config
        // 6. For INTER_FRAME: parse intra_only, reset_frame_context
        // 7. Parse refresh_frame_flags (8 bits)
        // 8. Parse ref_frame_idx for active references
        // 9. Parse frame size, render size
        
        // Placeholder implementation
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "VP9 frame header parsing not yet implemented"
        ))
    }
}

/// Simple bitstream reader for bit-level parsing
struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7, position within current byte from MSB
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }
    
    /// Read n bits (up to 32) as u32, MSB first
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
        assert_eq!(reader.read_bits(4).unwrap(), 0b1111); // 11 (from first byte) + 11 (from second)
    }
    
    #[test]
    fn test_updated_slots() {
        let header = Vp9FrameHeader {
            frame_type: Vp9FrameType::InterFrame,
            intra_only: false,
            refresh_frame_flags: 0b00000101, // slots 0 and 2
            ref_frame_idx: [1, 2, 3],
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
        };
        
        let slots = header.get_updated_slots();
        assert_eq!(slots, vec![0, 2]);
    }
    
    #[test]
    fn test_updates_slot() {
        let header = Vp9FrameHeader {
            frame_type: Vp9FrameType::KeyFrame,
            intra_only: false,
            refresh_frame_flags: 0xFF, // all slots
            ref_frame_idx: [0; 3],
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
        };
        
        for i in 0..8 {
            assert!(header.updates_slot(i));
        }
    }
    
    #[test]
    fn test_is_random_access_point() {
        // Test keyframe
        let keyframe = Vp9FrameHeader {
            frame_type: Vp9FrameType::KeyFrame,
            intra_only: false,
            refresh_frame_flags: 0xFF,
            ref_frame_idx: [0; 3],
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
        };
        assert!(keyframe.is_random_access_point());
        
        // Test intra-only frame with full refresh
        let intra_only = Vp9FrameHeader {
            frame_type: Vp9FrameType::InterFrame,
            intra_only: true,
            refresh_frame_flags: 0xFF,
            ref_frame_idx: [0; 3],
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
        };
        assert!(intra_only.is_random_access_point());
        
        // Test regular inter frame
        let inter = Vp9FrameHeader {
            frame_type: Vp9FrameType::InterFrame,
            intra_only: false,
            refresh_frame_flags: 0x01,
            ref_frame_idx: [1, 2, 3],
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
        };
        assert!(!inter.is_random_access_point());
    }
}
