//! Simplified VP8 frame header parser for reference frame tracking
//!
//! This module provides minimal parsing of VP8 frame headers to extract
//! reference frame information needed for pre-roll frame calculation.

use std::io;

/// VP8 frame types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vp8FrameType {
    KeyFrame = 0,
    InterFrame = 1,
}

/// VP8 reference frame slots (only 3 in VP8)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Vp8RefFrame {
    LastFrame = 0,
    GoldenFrame = 1,
    AltRefFrame = 2,
}

/// Parsed VP8 frame header with reference frame information
#[derive(Debug, Clone)]
pub struct Vp8FrameHeader {
    /// Frame type (KEY_FRAME, INTER_FRAME)
    pub frame_type: Vp8FrameType,
    
    /// Whether this frame updates the LAST_FRAME reference buffer
    pub refresh_last_frame: bool,
    
    /// Whether this frame updates the GOLDEN_FRAME reference buffer
    pub refresh_golden_frame: bool,
    
    /// Whether this frame updates the ALTREF_FRAME reference buffer
    pub refresh_alternate_frame: bool,
    
    /// For inter frames: which reference frame is used for LAST
    /// (can copy from GOLDEN or ALTREF)
    pub copy_buffer_to_last: Option<Vp8RefFrame>,
    
    /// For inter frames: which reference frame is used for GOLDEN
    /// (can copy from LAST or ALTREF)
    pub copy_buffer_to_golden: Option<Vp8RefFrame>,
    
    /// For inter frames: which reference frame is used for ALTREF
    /// (can copy from LAST or GOLDEN)
    pub copy_buffer_to_alternate: Option<Vp8RefFrame>,
}

impl Vp8FrameHeader {
    /// Get which reference frame slots this frame updates (writes to)
    ///
    /// Returns a Vec of updated slot indices (0=LAST, 1=GOLDEN, 2=ALTREF)
    pub fn get_updated_slots(&self) -> Vec<u8> {
        let mut slots = Vec::new();
        if self.refresh_last_frame {
            slots.push(Vp8RefFrame::LastFrame as u8);
        }
        if self.refresh_golden_frame {
            slots.push(Vp8RefFrame::GoldenFrame as u8);
        }
        if self.refresh_alternate_frame {
            slots.push(Vp8RefFrame::AltRefFrame as u8);
        }
        slots
    }
    
    /// Check if this frame updates a specific slot
    ///
    /// Slot indices: 0=LAST, 1=GOLDEN, 2=ALTREF
    pub fn updates_slot(&self, slot: u8) -> bool {
        match slot {
            0 => self.refresh_last_frame,
            1 => self.refresh_golden_frame,
            2 => self.refresh_alternate_frame,
            _ => false,
        }
    }
    
    /// Check if this is a true IDR-like keyframe
    ///
    /// VP8 keyframes typically refresh all reference buffers, making them
    /// true random access points (similar to H.264 IDR frames).
    pub fn is_idr_like(&self) -> bool {
        self.frame_type == Vp8FrameType::KeyFrame
    }
    
    /// Get which reference frame slots this frame depends on
    ///
    /// For keyframes, returns empty vec (no dependencies).
    /// For inter frames, returns the slots that are actually referenced.
    pub fn get_dependency_slots(&self) -> Vec<u8> {
        if self.frame_type == Vp8FrameType::KeyFrame {
            return Vec::new();
        }
        
        // VP8 inter frames can reference any of the 3 slots
        // Without parsing the actual prediction mode, we conservatively
        // assume all three slots might be used
        vec![
            Vp8RefFrame::LastFrame as u8,
            Vp8RefFrame::GoldenFrame as u8,
            Vp8RefFrame::AltRefFrame as u8,
        ]
    }
    
    /// Check if all reference buffers are refreshed
    ///
    /// When true, this frame acts as a clean break point
    pub fn refreshes_all_buffers(&self) -> bool {
        self.refresh_last_frame 
            && self.refresh_golden_frame 
            && self.refresh_alternate_frame
    }
    
    /// Parse VP8 frame header from raw frame data
    ///
    /// This is a simplified parser that extracts only the reference frame information
    /// needed for pre-roll calculation. It does not perform full VP8 decoding.
    pub fn parse(_data: &[u8]) -> io::Result<Self> {
        // TODO: Implement actual VP8 frame header parsing
        // VP8 frame format:
        // 1. Frame tag (3 bytes):
        //    - Bit 0: frame_type (0=keyframe, 1=inter)
        //    - Bits 1-3: version
        //    - Bit 4: show_frame
        //    - Bits 5-23: first_part_size (19 bits)
        // 2. For keyframe: start code (3 bytes) + width + height
        // 3. Uncompressed data chunk header
        // 4. Parse refresh flags (only for inter frames):
        //    - refresh_last_frame (1 bit)
        //    - refresh_golden_frame (1 bit)
        //    - refresh_alternate_frame (1 bit)
        //    - copy_buffer_to_last (2 bits)
        //    - copy_buffer_to_golden (2 bits)
        //    - copy_buffer_to_alternate (2 bits)
        
        // Placeholder implementation
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "VP8 frame header parsing not yet implemented"
        ))
    }
}

/// Simple bitstream reader for bit-level parsing
struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7, position within current byte from LSB for VP8
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }
    
    /// Read n bits (up to 32) as u32, LSB first (VP8 convention)
    fn read_bits(&mut self, n: u8) -> io::Result<u32> {
        if n > 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot read more than 32 bits at once"
            ));
        }
        
        let mut result = 0u32;
        let mut bits_read = 0;
        
        while bits_read < n {
            if self.byte_pos >= self.data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Not enough data for bit read"
                ));
            }
            
            let bits_in_current_byte = 8 - self.bit_pos;
            let bits_to_read = (n - bits_read).min(bits_in_current_byte);
            
            let byte = self.data[self.byte_pos];
            let mask = ((1u8 << bits_to_read) - 1) << self.bit_pos;
            let bits = ((byte & mask) >> self.bit_pos) as u32;
            
            result |= bits << bits_read;
            
            self.bit_pos += bits_to_read;
            if self.bit_pos >= 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
            
            bits_read += bits_to_read;
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
    fn test_bit_reader_lsb() {
        // Test data: 0b10110011 = 179 decimal
        // Bits from LSB: bit0=1, bit1=1, bit2=0, bit3=0, bit4=1, bit5=1, bit6=0, bit7=1
        let data = [0b10110011];
        let mut reader = BitReader::new(&data);
        
        // Read bit 0: value 1
        assert_eq!(reader.read_bits(1).unwrap(), 1);
        
        // Read bits 1-2: values [1,0] -> LSB first means: 1 + 0<<1 = 1
        assert_eq!(reader.read_bits(2).unwrap(), 0b01);
        
        // Read bits 3-5: values [0,1,1] -> LSB first means: 0 + 1<<1 + 1<<2 = 6
        assert_eq!(reader.read_bits(3).unwrap(), 0b110);
    }
    
    #[test]
    fn test_updated_slots() {
        let header = Vp8FrameHeader {
            frame_type: Vp8FrameType::InterFrame,
            refresh_last_frame: true,
            refresh_golden_frame: false,
            refresh_alternate_frame: true,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        
        let slots = header.get_updated_slots();
        assert_eq!(slots, vec![0, 2]); // LAST and ALTREF
    }
    
    #[test]
    fn test_updates_slot() {
        let header = Vp8FrameHeader {
            frame_type: Vp8FrameType::KeyFrame,
            refresh_last_frame: true,
            refresh_golden_frame: true,
            refresh_alternate_frame: true,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        
        assert!(header.updates_slot(0)); // LAST
        assert!(header.updates_slot(1)); // GOLDEN
        assert!(header.updates_slot(2)); // ALTREF
        assert!(!header.updates_slot(3)); // Invalid slot
    }
    
    #[test]
    fn test_is_idr_like() {
        let keyframe = Vp8FrameHeader {
            frame_type: Vp8FrameType::KeyFrame,
            refresh_last_frame: true,
            refresh_golden_frame: true,
            refresh_alternate_frame: true,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        assert!(keyframe.is_idr_like());
        
        let inter = Vp8FrameHeader {
            frame_type: Vp8FrameType::InterFrame,
            refresh_last_frame: true,
            refresh_golden_frame: false,
            refresh_alternate_frame: false,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        assert!(!inter.is_idr_like());
    }
    
    #[test]
    fn test_refreshes_all_buffers() {
        let full_refresh = Vp8FrameHeader {
            frame_type: Vp8FrameType::InterFrame,
            refresh_last_frame: true,
            refresh_golden_frame: true,
            refresh_alternate_frame: true,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        assert!(full_refresh.refreshes_all_buffers());
        
        let partial_refresh = Vp8FrameHeader {
            frame_type: Vp8FrameType::InterFrame,
            refresh_last_frame: true,
            refresh_golden_frame: false,
            refresh_alternate_frame: true,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        assert!(!partial_refresh.refreshes_all_buffers());
    }
    
    #[test]
    fn test_get_dependency_slots() {
        // Keyframes have no dependencies
        let keyframe = Vp8FrameHeader {
            frame_type: Vp8FrameType::KeyFrame,
            refresh_last_frame: true,
            refresh_golden_frame: true,
            refresh_alternate_frame: true,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        assert_eq!(keyframe.get_dependency_slots(), Vec::<u8>::new());
        
        // Inter frames depend on all 3 slots (conservative assumption)
        let inter = Vp8FrameHeader {
            frame_type: Vp8FrameType::InterFrame,
            refresh_last_frame: true,
            refresh_golden_frame: false,
            refresh_alternate_frame: false,
            copy_buffer_to_last: None,
            copy_buffer_to_golden: None,
            copy_buffer_to_alternate: None,
        };
        assert_eq!(inter.get_dependency_slots(), vec![0, 1, 2]);
    }
}
